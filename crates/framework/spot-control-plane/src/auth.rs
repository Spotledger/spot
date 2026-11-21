//! Authentication and scope, centralised: the operator and host middleware, the operator tenant
//! scope handed to modules, the signed-command intake (the management API crate's verification
//! pieces, used as a library) and the host certificate authority.
//!
//! Every module receives an already-scoped handle ([`OperatorCall::scope`]); a module that filters
//! through it cannot list a tenant outside the operator's scope, and an out-of-scope tenant is the
//! same `cp.not_found` as a missing one (scope is not an oracle). A host's tenant comes from its
//! certificate ([`HostIdentity`]), never from a request body.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use sha2::{Digest, Sha256};
use spot_core::Clock;
use spot_mgmt_api::ports::{AuditOutcome, AuditRecord, AuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_mgmt_api::wire::{self, KeyDirectory};
use spot_signed_command::{
    CommandError, CommandVerifier, KeyId, NonceFuture, NonceStore, StoreError, TrustedKeys,
    VerifyPolicy,
};

use crate::config::OperatorConfig;
use crate::{CpCode, CpError};

/// The largest request body the control plane reads (the telemetry batch limit).
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// The longest `X-Spot-Command` header.
pub const MAX_COMMAND_HEADER: usize = 16 * 1024;

// ---------------------------------------------------------------------------------------------
// Operator scope
// ---------------------------------------------------------------------------------------------

/// The tenants an operator may touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TenantScope {
    /// Every tenant.
    All,
    /// These tenant ids.
    Ids(BTreeSet<String>),
}

impl TenantScope {
    /// From the configuration list: `["*"]` is all.
    #[must_use]
    pub fn from_config(tenants: &[String]) -> Self {
        if tenants.iter().any(|tenant| tenant == "*") {
            Self::All
        } else {
            Self::Ids(tenants.iter().cloned().collect())
        }
    }
}

/// The scoped handle a module uses for every tenant access.
#[derive(Clone, Debug)]
pub struct ScopedTenants {
    scope: TenantScope,
}

impl ScopedTenants {
    /// A handle for a scope.
    #[must_use]
    pub fn new(scope: TenantScope) -> Self {
        Self { scope }
    }

    /// Whether the tenant is in scope.
    #[must_use]
    pub fn allows(&self, tenant: &str) -> bool {
        match &self.scope {
            TenantScope::All => true,
            TenantScope::Ids(ids) => ids.contains(tenant),
        }
    }

    /// `Ok` when in scope, otherwise `cp.not_found`: the same answer as a missing tenant.
    ///
    /// # Errors
    /// [`CpCode::NotFound`].
    pub fn require(&self, tenant: &str) -> Result<(), CpError> {
        if self.allows(tenant) {
            Ok(())
        } else {
            Err(CpError::new(CpCode::NotFound))
        }
    }

    /// Keeps only the items whose tenant is in scope.
    #[must_use]
    pub fn retain<T>(&self, items: Vec<T>, tenant_of: impl Fn(&T) -> &str) -> Vec<T> {
        items
            .into_iter()
            .filter(|item| self.allows(tenant_of(item)))
            .collect()
    }

    /// Whether the operator is scoped to every tenant (needed for fleet-wide creates).
    #[must_use]
    pub fn is_all(&self) -> bool {
        self.scope == TenantScope::All
    }
}

/// A verified signed command that authorised this request.
#[derive(Clone, Debug)]
pub struct VerifiedCommand {
    /// The command id.
    pub id: String,
    /// The operation the command names (`cp.<operation id>`).
    pub op: String,
    /// The approval records the envelope carried (opaque).
    pub approvals: Vec<String>,
}

/// What the operator middleware gives a module: the principal, the scope, and for a mutation the
/// verified command. Read it with `axum::Extension<OperatorCall>`.
#[derive(Clone, Debug)]
pub struct OperatorCall {
    /// The principal name.
    pub principal: String,
    /// The scoped tenant handle.
    pub scope: ScopedTenants,
    /// The verified command, present for every mutation.
    pub command: Option<VerifiedCommand>,
    /// The request id (also in the response and the audit).
    pub request_id: String,
}

/// The request id of the current request, inserted by both middleware, so a module's error answers
/// carry the same id the audit has.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// A tenant host (or its agent), identified by its enrolled certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostIdentity {
    /// The tenant the host belongs to.
    pub tenant: String,
    /// The host id.
    pub host_id: String,
    /// The certificate's SHA-256 fingerprint.
    pub fingerprint: String,
}

fn valid_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl HostIdentity {
    /// Parses the subject `CN=spot-host/<tenant>/<host id>` a host certificate carries.
    #[must_use]
    pub fn from_peer(peer: &PeerCertificate) -> Option<Self> {
        let subject = peer.subject()?;
        let rest = subject.strip_prefix("CN=spot-host/")?;
        let (tenant, host_id) = rest.split_once('/')?;
        (valid_id(tenant) && valid_id(host_id)).then(|| Self {
            tenant: tenant.to_owned(),
            host_id: host_id.to_owned(),
            fingerprint: peer.fingerprint().to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Signed-command intake
// ---------------------------------------------------------------------------------------------

struct SharedNonces(Arc<dyn NonceStore>);

impl NonceStore for SharedNonces {
    fn check_and_insert<'a>(
        &'a self,
        key: &'a KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        self.0.check_and_insert(key, nonce, expires_at, now)
    }
}

/// Verifies the `X-Spot-Command` envelope of an operator mutation.
pub struct SignedIntake {
    verifier: CommandVerifier,
    keys: KeyDirectory,
    audience: String,
}

impl SignedIntake {
    /// Trusts these administrator keys; commands must name `audience`.
    #[must_use]
    pub fn new(
        administrator_keys: &[[u8; 32]],
        nonces: Arc<dyn NonceStore>,
        audience: &str,
    ) -> Self {
        let mut trusted = TrustedKeys::new();
        let mut keys: KeyDirectory = BTreeMap::new();
        for key in administrator_keys {
            let id = KeyId::from_public_key(key);
            if trusted.insert(id.clone(), key).is_ok() {
                keys.insert(id.as_str().to_owned(), *key);
            }
        }
        let policy = VerifyPolicy {
            max_skew: spot_mgmt_api::MAX_CLOCK_SKEW_SECS,
            max_lifetime: spot_mgmt_api::MAX_COMMAND_LIFETIME_SECS,
            min_signatures: 1,
        };
        Self {
            verifier: CommandVerifier::new(trusted, policy, Box::new(SharedNonces(nonces))),
            keys,
            audience: audience.to_owned(),
        }
    }

    /// Verifies the header against the request: the signature, audience and expiry; the nonce is
    /// consumed last, durably; and the command must bind this request: its params carry
    /// `body_sha256` (hex SHA-256 of the body) and `path`.
    ///
    /// # Errors
    /// `cp.signature_invalid`, `cp.replayed` or `cp.internal` (a nonce store failure refuses).
    pub async fn verify(
        &self,
        header: &str,
        body: &[u8],
        path: &str,
        now: i64,
    ) -> Result<VerifiedCommand, CpError> {
        let invalid = || CpError::with_reason(CpCode::SignatureInvalid, "cp.signature_invalid");
        if header.is_empty() || header.len() > MAX_COMMAND_HEADER {
            return Err(invalid());
        }
        let raw = URL_SAFE_NO_PAD
            .decode(header.as_bytes())
            .map_err(|_| invalid())?;
        let envelope = wire::decode(&raw).map_err(|_| invalid())?;
        let signed = envelope.to_signed(&self.keys).map_err(|_| invalid())?;
        let verified = self
            .verifier
            .verify(&signed, &self.audience, now)
            .await
            .map_err(|error| match error {
                CommandError::Replay => CpError::with_reason(CpCode::Replayed, "cp.replayed"),
                CommandError::Store(_) | CommandError::InvalidKey => {
                    CpError::with_reason(CpCode::Internal, "cp.nonce_store_unavailable")
                }
                _ => invalid(),
            })?;
        let params = verified.command.params();
        let digest = sha256_hex(body);
        let binds = params.get("body_sha256").and_then(|v| v.as_str()) == Some(digest.as_str())
            && params.get("path").and_then(|v| v.as_str()) == Some(path);
        if !binds {
            return Err(CpError::with_reason(
                CpCode::SignatureInvalid,
                "cp.signature_unbound",
            ));
        }
        Ok(VerifiedCommand {
            id: verified.command.id().to_owned(),
            op: verified.command.op().to_owned(),
            approvals: envelope.approvals,
        })
    }
}

/// Lower-case hex SHA-256.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------------------------

/// State shared by both middleware: the clock, the audit sink and the request counter.
pub struct AuthShared {
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditSink>,
    counter: AtomicU64,
}

impl AuthShared {
    /// New shared state.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, audit: Arc<dyn AuditSink>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            audit,
            counter: AtomicU64::new(1),
        })
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn next_id(&self) -> String {
        format!("req-{:016x}", self.counter.fetch_add(1, Ordering::Relaxed))
    }
}

/// State of the operator middleware.
pub struct OperatorAuth {
    /// Shared state.
    pub shared: Arc<AuthShared>,
    /// The allow-list.
    pub operators: Vec<OperatorConfig>,
    /// The signed-command intake.
    pub intake: SignedIntake,
}

/// State of the host middleware.
pub struct HostAuth {
    /// Shared state.
    pub shared: Arc<AuthShared>,
    /// Revoked certificates.
    pub revocations: Arc<Revocations>,
}

struct Audit<'a> {
    shared: &'a AuthShared,
    request_id: &'a str,
    principal: Option<String>,
    fingerprint: Option<String>,
    action: String,
    command_id: Option<String>,
}

impl Audit<'_> {
    /// Writes the record; false when the sink failed.
    async fn write(&self, status: u16, reason: Option<String>) -> bool {
        let outcome = if status < 400 {
            AuditOutcome::Allowed
        } else {
            AuditOutcome::Refused
        };
        let record = AuditRecord {
            request_id: self.request_id.to_owned(),
            principal: self.principal.clone(),
            certificate_fingerprint: self.fingerprint.clone(),
            action: self.action.clone(),
            operation: None,
            risk: None,
            outcome,
            reason,
            command_id: self.command_id.clone(),
            at: self.shared.now(),
        };
        self.shared.audit.record(record).await.is_ok()
    }
}

fn is_mutation(method: &axum::http::Method) -> bool {
    !matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    )
}

/// Audits a refusal that happened before the module ran, and answers it.
async fn refuse(audit: Audit<'_>, error: CpError) -> Response {
    let _ = audit
        .write(error.code.status().as_u16(), Some(error.reason.to_owned()))
        .await;
    error.into_response_with(audit.request_id)
}

/// The operator listener's middleware: certificate, allow-list, scope and, for a mutation, the
/// signed command (bound to the body by its digest). The audit record is written before the
/// response is returned; a sink failure turns a mutation's success into `cp.internal`.
pub async fn operator_layer(
    State(auth): State<Arc<OperatorAuth>>,
    request: Request,
    next: Next,
) -> Response {
    let shared = &auth.shared;
    let request_id = shared.next_id();
    let (mut parts, body) = request.into_parts();
    let mut audit = Audit {
        shared,
        request_id: &request_id,
        principal: None,
        fingerprint: None,
        action: format!("{} {}", parts.method, parts.uri.path()),
        command_id: None,
    };
    let Some(peer) = parts.extensions.get::<PeerCertificate>().cloned() else {
        return refuse(
            audit,
            CpError::with_reason(CpCode::Unauthenticated, "cp.no_certificate"),
        )
        .await;
    };
    audit.fingerprint = Some(peer.fingerprint().to_owned());
    let Some(operator) = peer
        .subject()
        .and_then(|subject| auth.operators.iter().find(|entry| entry.subject == subject))
    else {
        return refuse(
            audit,
            CpError::with_reason(CpCode::Forbidden, "cp.not_an_operator"),
        )
        .await;
    };
    audit.principal = Some(operator.principal.clone());
    let mutation = is_mutation(&parts.method);
    let mut command = None;
    let body = if mutation {
        let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return refuse(
                    audit,
                    CpError::with_reason(CpCode::BodyTooLarge, "cp.body_too_large"),
                )
                .await;
            }
        };
        let header = parts
            .headers
            .get("x-spot-command")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        match auth
            .intake
            .verify(header, &bytes, parts.uri.path(), shared.now())
            .await
        {
            Ok(verified) => {
                audit.command_id = Some(verified.id.clone());
                command = Some(verified);
            }
            Err(error) => return refuse(audit, error).await,
        }
        Body::from(bytes)
    } else {
        body
    };
    parts.extensions.insert(OperatorCall {
        principal: operator.principal.clone(),
        scope: ScopedTenants::new(TenantScope::from_config(&operator.tenants)),
        command,
        request_id: request_id.clone(),
    });
    let response = next.run(Request::from_parts(parts, body)).await;
    let status = response.status().as_u16();
    let written = audit.write(status, None).await;
    if !written && mutation && status < 400 {
        return CpError::with_reason(CpCode::Internal, "cp.audit_unavailable")
            .into_response_with(&request_id);
    }
    response
}

/// The host listener's middleware: certificate, host identity from its subject, revocation
/// (checked on every request, so a revoked certificate is refused immediately), and audit.
pub async fn host_layer(
    State(auth): State<Arc<HostAuth>>,
    request: Request,
    next: Next,
) -> Response {
    let shared = &auth.shared;
    let request_id = shared.next_id();
    let (mut parts, body) = request.into_parts();
    let mut audit = Audit {
        shared,
        request_id: &request_id,
        principal: None,
        fingerprint: None,
        action: format!("{} {}", parts.method, parts.uri.path()),
        command_id: None,
    };
    let Some(peer) = parts.extensions.get::<PeerCertificate>().cloned() else {
        return refuse(
            audit,
            CpError::with_reason(CpCode::Unauthenticated, "cp.no_certificate"),
        )
        .await;
    };
    audit.fingerprint = Some(peer.fingerprint().to_owned());
    let Some(identity) = HostIdentity::from_peer(&peer) else {
        return refuse(
            audit,
            CpError::with_reason(CpCode::Unauthenticated, "cp.not_a_host"),
        )
        .await;
    };
    if auth.revocations.is_revoked(peer.fingerprint()) {
        return refuse(
            audit,
            CpError::with_reason(CpCode::Unauthenticated, "cp.certificate_revoked"),
        )
        .await;
    }
    audit.principal = Some(format!("host:{}/{}", identity.tenant, identity.host_id));
    parts.extensions.insert(identity);
    parts.extensions.insert(RequestId(request_id.clone()));
    let mutation = is_mutation(&parts.method);
    let response = next.run(Request::from_parts(parts, body)).await;
    let status = response.status().as_u16();
    let written = audit.write(status, None).await;
    if !written && mutation && status < 400 {
        return CpError::with_reason(CpCode::Internal, "cp.audit_unavailable")
            .into_response_with(&request_id);
    }
    response
}

// ---------------------------------------------------------------------------------------------
// Host certificate authority
// ---------------------------------------------------------------------------------------------

/// Why the CA could not act.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaError {
    /// The key vault refused or the key is unusable.
    #[error("the CA key is unavailable")]
    Vault,
    /// A tenant or host id is not a lower-case identifier.
    #[error("the tenant or host id is not valid")]
    BadIdentity,
    /// The certificate could not be generated.
    #[error("the certificate could not be issued")]
    Issue,
    /// The presented certificate is revoked.
    #[error("the certificate is revoked")]
    Revoked,
    /// The audit sink failed: nothing is issued or revoked without its record.
    #[error("the audit record could not be written")]
    Audit,
}

/// Where the CA key lives. The key never leaves this abstraction except as the PEM handed to the
/// signer at start; it is never logged. A file with mode 0600 now; an HSM or KMS later.
pub trait KeyVault: Send + Sync {
    /// The CA private key in PEM.
    ///
    /// # Errors
    /// [`CaError::Vault`].
    fn ca_key_pem(&self) -> Result<String, CaError>;
}

/// A key file; on Unix any group or other permission refuses the load.
pub struct FileKeyVault {
    path: PathBuf,
}

impl FileKeyVault {
    /// A vault over `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl KeyVault for FileKeyVault {
    fn ca_key_pem(&self) -> Result<String, CaError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&self.path)
                .map_err(|_| CaError::Vault)?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(CaError::Vault);
            }
        }
        std::fs::read_to_string(&self.path).map_err(|_| CaError::Vault)
    }
}

/// The revoked certificate fingerprints, optionally persisted one per line.
pub struct Revocations {
    revoked: RwLock<BTreeSet<String>>,
    file: Option<PathBuf>,
}

impl Revocations {
    /// Loads the list from `file` when present (a missing file is an empty list).
    #[must_use]
    pub fn load(file: Option<PathBuf>) -> Self {
        let mut set = BTreeSet::new();
        if let Some(path) = &file
            && let Ok(text) = std::fs::read_to_string(path)
        {
            set.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned),
            );
        }
        Self {
            revoked: RwLock::new(set),
            file,
        }
    }

    /// Whether the fingerprint is revoked. An unreadable list fails closed (everything revoked).
    #[must_use]
    pub fn is_revoked(&self, fingerprint: &str) -> bool {
        match self.revoked.read() {
            Ok(set) => set.contains(fingerprint),
            Err(_) => true,
        }
    }

    fn add(&self, fingerprint: &str) -> bool {
        let Ok(mut set) = self.revoked.write() else {
            return false;
        };
        if set.insert(fingerprint.to_owned())
            && let Some(path) = &self.file
        {
            use std::io::Write as _;
            let written = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| writeln!(file, "{fingerprint}"));
            if written.is_err() {
                set.remove(fingerprint);
                return false;
            }
        }
        true
    }
}

/// A freshly issued host or agent certificate.
#[derive(Clone)]
pub struct IssuedHostCert {
    /// The certificate (PEM).
    pub certificate_pem: String,
    /// The private key (PEM): shown once, never stored by the CA.
    pub private_key_pem: String,
    /// The SHA-256 fingerprint.
    pub fingerprint: String,
    /// Expiry, unix seconds.
    pub not_after: i64,
    /// The identity in the certificate.
    pub identity: HostIdentity,
}

impl std::fmt::Debug for IssuedHostCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedHostCert")
            .field("fingerprint", &self.fingerprint)
            .field("not_after", &self.not_after)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// The host certificate authority.
pub struct HostCa {
    params: CertificateParams,
    key: KeyPair,
    validity_days: u32,
    revocations: Arc<Revocations>,
    audit: Arc<dyn AuditSink>,
    clock: Arc<dyn Clock>,
}

impl HostCa {
    /// Builds the CA from its key (through the vault) and common name. The issuer certificate is
    /// rebuilt from the same name and key, which is what a verifier checks.
    ///
    /// # Errors
    /// [`CaError::Vault`] for an unusable key.
    pub fn new(
        common_name: &str,
        vault: &dyn KeyVault,
        validity_days: u32,
        revocations: Arc<Revocations>,
        audit: Arc<dyn AuditSink>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, CaError> {
        let pem = vault.ca_key_pem()?;
        let key = KeyPair::from_pem(&pem).map_err(|_| CaError::Vault)?;
        let mut params =
            CertificateParams::new(Vec::<String>::new()).map_err(|_| CaError::Issue)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        Ok(Self {
            params,
            key,
            validity_days,
            revocations,
            audit,
            clock,
        })
    }

    /// The revocation list the listener consults.
    #[must_use]
    pub fn revocations(&self) -> Arc<Revocations> {
        Arc::clone(&self.revocations)
    }

    async fn record(&self, action: &str, principal: &str, fingerprint: &str, ok: bool) -> bool {
        let record = AuditRecord {
            request_id: format!("ca-{fingerprint}"),
            principal: Some(principal.to_owned()),
            certificate_fingerprint: Some(fingerprint.to_owned()),
            action: action.to_owned(),
            operation: None,
            risk: None,
            outcome: if ok {
                AuditOutcome::Allowed
            } else {
                AuditOutcome::Failed
            },
            reason: None,
            command_id: None,
            at: self.clock.now_utc().unix_timestamp(),
        };
        self.audit.record(record).await.is_ok()
    }

    /// Issues a client certificate for a host (default validity 30 days). The issuance is audited;
    /// if the audit fails nothing is returned.
    ///
    /// # Errors
    /// [`CaError`].
    pub async fn issue(&self, tenant: &str, host_id: &str) -> Result<IssuedHostCert, CaError> {
        if !valid_id(tenant) || !valid_id(host_id) {
            return Err(CaError::BadIdentity);
        }
        let now = self.clock.now_utc().unix_timestamp();
        let not_after = now + i64::from(self.validity_days) * 86_400;
        let mut params =
            CertificateParams::new(Vec::<String>::new()).map_err(|_| CaError::Issue)?;
        params
            .distinguished_name
            .push(DnType::CommonName, format!("spot-host/{tenant}/{host_id}"));
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before =
            time::OffsetDateTime::from_unix_timestamp(now - 60).map_err(|_| CaError::Issue)?;
        params.not_after =
            time::OffsetDateTime::from_unix_timestamp(not_after).map_err(|_| CaError::Issue)?;
        let leaf_key = KeyPair::generate().map_err(|_| CaError::Issue)?;
        let issuer = Issuer::from_params(&self.params, &self.key);
        let certificate = params
            .signed_by(&leaf_key, &issuer)
            .map_err(|_| CaError::Issue)?;
        let fingerprint = PeerCertificate::from_der(certificate.der().as_ref())
            .fingerprint()
            .to_owned();
        if !self
            .record(
                "cp.host_ca.issue",
                &format!("{tenant}/{host_id}"),
                &fingerprint,
                true,
            )
            .await
        {
            return Err(CaError::Audit);
        }
        Ok(IssuedHostCert {
            certificate_pem: certificate.pem(),
            private_key_pem: leaf_key.serialize_pem(),
            fingerprint: fingerprint.clone(),
            not_after,
            identity: HostIdentity {
                tenant: tenant.to_owned(),
                host_id: host_id.to_owned(),
                fingerprint,
            },
        })
    }

    /// Renews over the existing certificate: the presented (TLS-verified, not revoked) identity
    /// gets a new certificate with the same tenant and host id.
    ///
    /// # Errors
    /// [`CaError::Revoked`] when the presented certificate is revoked; others as `issue`.
    pub async fn renew(&self, presented: &HostIdentity) -> Result<IssuedHostCert, CaError> {
        if self.revocations.is_revoked(&presented.fingerprint) {
            return Err(CaError::Revoked);
        }
        self.issue(&presented.tenant, &presented.host_id).await
    }

    /// Revokes a certificate by fingerprint. Audited; fails when the record cannot be written.
    ///
    /// # Errors
    /// [`CaError::Audit`] or [`CaError::Issue`] (the list could not be written).
    pub async fn revoke(&self, fingerprint: &str, by: &str) -> Result<(), CaError> {
        if !self.revocations.add(fingerprint) {
            return Err(CaError::Issue);
        }
        if self
            .record("cp.host_ca.revoke", by, fingerprint, true)
            .await
        {
            Ok(())
        } else {
            Err(CaError::Audit)
        }
    }
}
