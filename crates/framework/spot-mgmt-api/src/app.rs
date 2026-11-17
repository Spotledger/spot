//! The shared state of the router, the per-request trace and the one place where every outcome
//! is audited and turned into a response.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use rustls::pki_types::CertificateDer;
use serde_json::Value;
use spot_core::Clock;
use spot_mgmt_ops::{Catalogue, Risk};
use spot_signed_command::{
    CommandVerifier, KeyId, NonceFuture, NonceStore, StoreError, TrustedKeys, VerifyPolicy,
};
use tokio::sync::watch;

use crate::error::{MgmtCode, MgmtError};
use crate::handlers::HandlerRegistry;
use crate::lifecycle::{CommandStore, OperationLocks};
use crate::ports::{
    ApprovalCheck, AuditOutcome, AuditRecord, AuditSink, OperatorAllowList, OperatorEntry,
};
use crate::tls::PeerCertificate;
use crate::wire::KeyDirectory;

/// The longest clock skew the verifier tolerates, either way: 2 minutes (contract).
pub const MAX_CLOCK_SKEW_SECS: i64 = 120;
/// The longest command lifetime: 10 minutes (contract).
pub const MAX_COMMAND_LIFETIME_SECS: i64 = 600;
/// The request body limit: 64 KiB, applied before any parse.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Everything the server needs, supplied by the host.
pub struct Deps {
    /// The operation catalogue.
    pub catalogue: Arc<Catalogue>,
    /// Operation id to handler.
    pub handlers: HandlerRegistry,
    /// The durable, fail-closed nonce store (a store error refuses the command).
    pub nonces: Arc<dyn NonceStore>,
    /// Trusted administrator public keys (Ed25519, 32 bytes each).
    pub administrator_keys: Vec<[u8; 32]>,
    /// The operator CA bundle (DER), used by [`Deps::tls_config`].
    pub operator_ca: Vec<CertificateDer<'static>>,
    /// The operator subject allow-list.
    pub operators: OperatorAllowList,
    /// The approval records check.
    pub approvals: Arc<dyn ApprovalCheck>,
    /// The clock: the only way the server reads the time.
    pub clock: Arc<dyn Clock>,
    /// The audit sink.
    pub audit: Arc<dyn AuditSink>,
    /// The command lifecycle store.
    pub commands: Arc<dyn CommandStore>,
    /// This site's id: the audience every signed command must name.
    pub site_id: String,
}

impl Deps {
    /// The TLS configuration for this deployment's server certificate.
    ///
    /// # Errors
    /// See [`crate::tls::tls_config`].
    pub fn tls_config(
        &self,
        chain: Vec<CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> Result<rustls::ServerConfig, crate::tls::TlsConfigError> {
        crate::tls::tls_config(chain, key, &self.operator_ca)
    }
}

/// Delegates to a shared nonce store (the verifier owns a `Box`).
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

/// A running command's cancel switch.
pub(crate) struct RunningEntry {
    pub(crate) cancel: watch::Sender<bool>,
    pub(crate) cancellable: bool,
}

/// The router state.
pub(crate) struct AppState {
    pub(crate) catalogue: Arc<Catalogue>,
    pub(crate) handlers: HandlerRegistry,
    pub(crate) verifier: CommandVerifier,
    pub(crate) keys: KeyDirectory,
    pub(crate) operators: OperatorAllowList,
    pub(crate) approvals: Arc<dyn ApprovalCheck>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) audit: Arc<dyn AuditSink>,
    pub(crate) commands: Arc<dyn CommandStore>,
    pub(crate) site_id: String,
    pub(crate) locks: Arc<OperationLocks>,
    pub(crate) running: Mutex<HashMap<String, RunningEntry>>,
    counter: AtomicU64,
}

impl AppState {
    pub(crate) fn new(deps: Deps) -> Self {
        let mut trusted = TrustedKeys::new();
        let mut keys: KeyDirectory = BTreeMap::new();
        for public_key in &deps.administrator_keys {
            let id = KeyId::from_public_key(public_key);
            // An unusable key is dropped: an empty trust set verifies nothing (fail closed).
            if trusted.insert(id.clone(), public_key).is_ok() {
                keys.insert(id.as_str().to_owned(), *public_key);
            } else {
                tracing::error!("an administrator key is not a valid Ed25519 key and was ignored");
            }
        }
        let policy = VerifyPolicy {
            max_skew: MAX_CLOCK_SKEW_SECS,
            max_lifetime: MAX_COMMAND_LIFETIME_SECS,
            min_signatures: 1,
        };
        let verifier = CommandVerifier::new(trusted, policy, Box::new(SharedNonces(deps.nonces)));
        Self {
            catalogue: deps.catalogue,
            handlers: deps.handlers,
            verifier,
            keys,
            operators: deps.operators,
            approvals: deps.approvals,
            clock: deps.clock,
            audit: deps.audit,
            commands: deps.commands,
            site_id: deps.site_id,
            locks: OperationLocks::new(),
            running: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
        }
    }

    pub(crate) fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn next_request_id(&self) -> String {
        format!("req-{:016x}", self.counter.fetch_add(1, Ordering::Relaxed))
    }

    /// Authenticates the request: a verified peer certificate, mapped to an allow-listed operator.
    pub(crate) fn authenticate(
        &self,
        trace: &mut Trace,
        parts: &Parts,
    ) -> Result<OperatorEntry, MgmtError> {
        let Some(peer) = parts.extensions.get::<PeerCertificate>() else {
            return Err(MgmtError::with_reason(
                MgmtCode::Unauthenticated,
                "mgmt.no_client_certificate",
            ));
        };
        trace.fingerprint = Some(peer.fingerprint().to_owned());
        let entry = peer
            .subject()
            .and_then(|subject| self.operators.resolve(subject))
            .ok_or_else(|| {
                MgmtError::with_reason(MgmtCode::Forbidden, "mgmt.operator_not_allow_listed")
            })?;
        trace.principal = Some(entry.principal.clone());
        Ok(entry.clone())
    }
}

/// What one request has learned about itself, for its audit record.
pub(crate) struct Trace {
    pub(crate) request_id: String,
    pub(crate) action: String,
    pub(crate) fingerprint: Option<String>,
    pub(crate) principal: Option<String>,
    pub(crate) operation: Option<String>,
    pub(crate) risk: Option<Risk>,
    pub(crate) command_id: Option<String>,
}

impl Trace {
    pub(crate) fn new(app: &AppState, action: &str) -> Self {
        Self {
            request_id: app.next_request_id(),
            action: action.to_owned(),
            fingerprint: None,
            principal: None,
            operation: None,
            risk: None,
            command_id: None,
        }
    }
}

/// Work to start once the acceptance has been audited.
pub(crate) struct Launch {
    pub(crate) guard: crate::lifecycle::LockGuard,
    pub(crate) run: Box<dyn FnOnce(String, crate::lifecycle::LockGuard) -> LaunchFuture + Send>,
    pub(crate) abort: Box<dyn FnOnce() -> LaunchFuture + Send>,
}

pub(crate) type LaunchFuture = crate::ports::BoxFuture<'static, ()>;

/// A successful request, before audit.
pub(crate) struct Success {
    pub(crate) status: StatusCode,
    pub(crate) body: Value,
    pub(crate) launch: Option<Launch>,
}

impl Success {
    pub(crate) fn ok(body: Value) -> Self {
        Self {
            status: StatusCode::OK,
            body,
            launch: None,
        }
    }
}

/// Audits the outcome, then answers. The audit call happens before the response is built; when it
/// fails, a success of an operation above `read` becomes `mgmt.internal` and a command that was
/// about to start is aborted instead.
pub(crate) async fn finish(
    app: &AppState,
    trace: Trace,
    result: Result<Success, MgmtError>,
) -> Response {
    let (outcome, reason) = match &result {
        Ok(_) => (AuditOutcome::Allowed, None),
        Err(error) => (AuditOutcome::Refused, Some(error.reason.to_owned())),
    };
    let record = AuditRecord {
        request_id: trace.request_id.clone(),
        principal: trace.principal.clone(),
        certificate_fingerprint: trace.fingerprint.clone(),
        action: trace.action.clone(),
        operation: trace.operation.clone(),
        risk: trace.risk,
        outcome,
        reason,
        command_id: trace.command_id.clone(),
        at: app.now(),
    };
    let audited = app.audit.record(record).await;
    match result {
        Err(error) => error.into_response_with(&trace.request_id),
        Ok(success) => {
            let above_read = trace.risk.is_some_and(|risk| risk > Risk::Read);
            match audited {
                Ok(audit_ref) => {
                    if let Some(launch) = success.launch {
                        (launch.run)(audit_ref, launch.guard).await;
                    }
                    (success.status, Json(success.body)).into_response()
                }
                Err(_) if above_read => {
                    if let Some(launch) = success.launch {
                        (launch.abort)().await;
                        drop(launch.guard);
                    }
                    MgmtError::with_reason(MgmtCode::Internal, "mgmt.audit_unavailable")
                        .into_response_with(&trace.request_id)
                }
                Err(_) => (success.status, Json(success.body)).into_response(),
            }
        }
    }
}
