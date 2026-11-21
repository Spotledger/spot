//! Support-access and break-glass wiring (WP-ADM-11.T4): provides the ports the break-glass host
//! (`services/break_glass`, WP-ADM-11.T2) reads, each from the real component of this site where
//! one exists, and a fail-closed refusal with a catalogued error where none does.
//!
//! # What is provided, and from what
//!
//! | Port | Provided from | Without it |
//! |---|---|---|
//! | [`EventLog`] | the site's hash-chained audit store (`spot-audit-store`), one chain of its own: scope `break_glass:<site>` | the port is not provided; the break-glass check names `audit chain` |
//! | [`ObjectStore`] and [`Retire`] | the site's file store (`spot-files`), under the private prefix `break_glass/`; no overwrite, ever | not provided; names `object store` |
//! | [`Sealer`] | the secrets service's vault ([`SecretsHost`]) | not provided; names `sealer` |
//! | [`Duties`] | the site's permission engine: the duty is an app permission of the same name | not provided; names `duty check` |
//! | [`AgentBridge`] | nothing in this host reaches a host agent yet: [`RefusingBridge`] answers `break_glass_host.not_wired` to every kill, so a revoke closes the session and the sweep keeps retrying | (always present) |
//! | [`Retention`] | `retention_days` of this module's table, when set; otherwise not provided and break-glass uses its own fallback | not provided |
//!
//! The approval verifier, the agent dispatcher and the agent's account provider are parts of the
//! control plane and of the host agent, which are separate processes with their own `main`; this
//! crate has no path to them, so nothing here wires them (see the report's open questions).
//!
//! # Fail closed
//!
//! A port whose real component is absent is **not provided at all**, so `break_glass` reports the
//! part missing, every read answers `break_glass_host.not_wired` and no session can be authorised.
//! An adapter that fails answers the matching `break_glass_host.*` refusal, never an allow: a duty
//! check that errors is a "no", an object that exists is never overwritten, a recording that cannot
//! be sealed is not stored in the clear.
//!
//! # Setting
//!
//! `[services.support_access_wiring]` (all optional, unknown keys refused): `enabled` (default
//! true), `retention_days` (1 to 3650; unset leaves the retention to `[services.break_glass]`) and
//! `max_object_bytes` (1 to 1 GiB, default 256 MiB: a recording is stored as the hex of its bytes
//! under the vault, so it takes twice its size).
//!
//! # Level
//!
//! Optional. The start-up check fails (and the site reports itself degraded) while the audit
//! chain, the object store, the sealer or the duty check is missing; the agent bridge and the
//! retention are not part of it.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use spot_app_api::prelude::RecordId;
use spot_app_permissions::SnapshotAppPermissions;
use spot_audit_chain::{
    AuditChain, AuditError, AuditRecord, AuditStore, ChainEntry, CheckpointPolicy,
    CheckpointSigner, PayloadValue, Subject,
};
use spot_audit_store::PgAuditStore;
use spot_break_glass::BreakGlassError;
use spot_core::{Audience, Clock, SiteId};
use spot_document::engine::{AppPermissionCheck, PrincipalSource};
use spot_document::secrets::SecretVault;
use spot_files::error::FileError;
use spot_files::key::{FileKey, FileKeyPrefix, Visibility};
use spot_files::store::{FileStore, PutOptions, body_from_vec};
use spot_http::api::service::{ApiError, RequestCtx};
use spot_postgres::PgBackend;
use spot_secrets::SecretString;
use spot_storage_contract::{Catalog, RecordId as StorageId};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::break_glass::{
    ADMIN_DUTY, AgentBridge, BoxFuture, Duties, EventLog, HostError, ObjectStore, Parts, Retention,
    Retire, Sealer,
};
use crate::services::secrets::SecretsHost;
use crate::services::step_up::session_is_current;
use crate::services::support_access::{CallerInfo, SupportCaller};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the services it reads from (secrets and the files service) and before
/// `break_glass` (1170), which reads the ports.
pub const ORDER: u32 = 1165;

/// The module and service name, and the `[services.<name>]` table it reads.
pub const NAME: &str = "support_access_wiring";

/// The prefix of this module's chain: one chain of its own, so the break-glass trail verifies on
/// its own and shares nothing with the identity or document trails.
pub const SCOPE_PREFIX: &str = "break_glass:";

/// The first segment under the private file prefix: everything this module stores lives under it.
pub const OBJECT_ROOT: &str = "break_glass";

/// The stable codes of this module (see `catalogue/support_access_wiring.toml`).
pub mod code {
    /// `[services.support_access_wiring]` has an unknown key or a value out of bounds.
    pub const SETTINGS_INVALID: &str = "support_access_wiring.settings_invalid";
    /// A real component a break-glass port needs is missing on this site.
    pub const PART_MISSING: &str = "support_access_wiring.part_missing";
}

/// `[services.support_access_wiring]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site wires the break-glass ports.
    pub enabled: bool,
    /// Days to keep recordings; `None` leaves it to `[services.break_glass]`.
    pub retention_days: Option<u32>,
    /// The largest object stored, in bytes (1 to 1 GiB).
    pub max_object_bytes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: None,
            max_object_bytes: 256 * 1024 * 1024,
        }
    }
}

impl Settings {
    /// Checks the bounds.
    ///
    /// # Errors
    ///
    /// A value out of bounds; the message names the key, never a value.
    pub fn validate(&self) -> Result<()> {
        let bad = |key: &str| {
            Error(format!(
                "{}: services.{NAME}.{key} is out of bounds",
                code::SETTINGS_INVALID
            ))
        };
        if let Some(days) = self.retention_days
            && !(1..=3650).contains(&days)
        {
            return Err(bad("retention_days"));
        }
        if self.max_object_bytes == 0 || self.max_object_bytes > 1 << 30 {
            return Err(bad("max_object_bytes"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The audit chain
// ---------------------------------------------------------------------------------------------

/// A signer that never signs: the chain's automatic checkpoints are off (policy `every: 0`), so
/// it is never asked; if it ever were, the refusal keeps an unsigned checkpoint from being written
/// (checkpoint signing needs the key of WP-SEC-03 and is not this module's).
struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }
    fn sign(&self, _message: &[u8]) -> std::result::Result<Vec<u8>, AuditError> {
        Err(AuditError::Signer("no checkpoint key is configured".into()))
    }
}

/// The chain scope of `site`'s break-glass events.
#[must_use]
pub fn scope_of(site: &str) -> String {
    format!("{SCOPE_PREFIX}{site}")
}

/// The [`EventLog`] over one chain of an [`AuditStore`]. An append is durable before it answers
/// `Ok`; every failure is the fixed `break_glass.audit_unavailable`, never the store's text.
pub struct ChainEventLog<S: AuditStore> {
    chain: AuditChain<S>,
}

impl<S: AuditStore> ChainEventLog<S> {
    /// The log of the chain `scope` in `store`.
    pub fn new(store: S, scope: &str) -> Self {
        Self {
            chain: AuditChain::new(
                store,
                scope,
                Arc::new(NoSigner),
                CheckpointPolicy { every: 0 },
            ),
        }
    }

    /// The chain, for verification by the caller.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<S> {
        &self.chain
    }
}

fn unavailable() -> HostError {
    HostError::Crate(BreakGlassError::AuditUnavailable)
}

impl<S: AuditStore + 'static> EventLog for ChainEventLog<S> {
    fn append<'a>(
        &'a self,
        event: spot_break_glass::BreakGlassEvent,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>> {
        Box::pin(async move {
            let mut payload = BTreeMap::new();
            payload.insert(
                "event".to_owned(),
                PayloadValue::Text(serde_json::to_string(&event).map_err(|_| unavailable())?),
            );
            let record = AuditRecord {
                kind: format!("break_glass.{}", event.kind),
                actor: "system".to_owned(),
                on_behalf_of: Some(format!("user:{}", event.who)),
                subject: Some(Subject {
                    doctype: "BreakGlassSession".to_owned(),
                    id: event.session,
                }),
                at: event.at.checked_mul(1_000_000).ok_or_else(unavailable)?,
                payload,
                masked: true,
            };
            self.chain
                .append(record)
                .await
                .map(|_| ())
                .map_err(|_| unavailable())
        })
    }

    fn entries<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<ChainEntry>, HostError>> {
        Box::pin(async move {
            self.chain
                .read_range(1, u64::MAX)
                .await
                .map_err(|_| unavailable())
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The object store
// ---------------------------------------------------------------------------------------------

/// How many keys one `list` page asks the file store for.
const LIST_PAGE: usize = 1000;

/// Encodes one break-glass key (`registry/<id>.json`) as a file key: file keys are lower case,
/// so `A` is written `_a` and `_` is written `__`. The mapping is reversible, so two different
/// ids never share an object.
fn encode_key(key: &str) -> std::result::Result<String, HostError> {
    let mut out = String::with_capacity(key.len() + OBJECT_ROOT.len() + 1);
    out.push_str(OBJECT_ROOT);
    for segment in key.split('/') {
        if segment.is_empty() {
            return Err(HostError::Invalid("the object key has an empty segment"));
        }
        out.push('/');
        for c in segment.chars() {
            match c {
                'a'..='z' | '0'..='9' | '.' | '-' => out.push(c),
                '_' => out.push_str("__"),
                'A'..='Z' => {
                    out.push('_');
                    out.push(c.to_ascii_lowercase());
                }
                _ => return Err(HostError::Invalid("the object key is not storable")),
            }
        }
    }
    Ok(out)
}

/// The inverse of [`encode_key`] for a key under the root; `None` for anything else.
fn decode_key(stored: &str) -> Option<String> {
    let rest = stored.strip_prefix(OBJECT_ROOT)?.strip_prefix('/')?;
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '_' {
            match chars.next()? {
                '_' => out.push('_'),
                l @ 'a'..='z' => out.push(l.to_ascii_uppercase()),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn file_key(key: &str) -> std::result::Result<FileKey, HostError> {
    FileKey::new(Visibility::Private, &encode_key(key)?)
        .map_err(|_| HostError::Invalid("the object key is not storable"))
}

/// The [`ObjectStore`] and [`Retire`] over the site's file store: private objects under
/// `break_glass/`, written once. [`ObjectStore::put_new`] never replaces an object; removal is
/// only on [`Retire`], which the host gives to the retention sweep alone.
pub struct FileObjects {
    store: Arc<dyn FileStore>,
    site: SiteId,
    max_bytes: u64,
}

impl FileObjects {
    /// Objects of `site` in `store`, none larger than `max_bytes`.
    pub fn new(store: Arc<dyn FileStore>, site: SiteId, max_bytes: u64) -> Self {
        Self {
            store,
            site,
            max_bytes,
        }
    }
}

impl ObjectStore for FileObjects {
    fn put_new<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>> {
        Box::pin(async move {
            let file = file_key(key)?;
            let options = PutOptions::new("application/octet-stream", self.max_bytes, false);
            match self
                .store
                .put(&self.site, file, body_from_vec(bytes), options)
                .await
            {
                Ok(_) => Ok(()),
                Err(FileError::AlreadyExists { .. }) => {
                    Err(HostError::Crate(BreakGlassError::AppendOnly))
                }
                Err(FileError::TooLarge { .. }) => {
                    Err(HostError::Invalid("the object is larger than the limit"))
                }
                Err(_) => Err(HostError::Store),
            }
        })
    }

    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<Vec<u8>>, HostError>> {
        Box::pin(async move {
            let file = file_key(key)?;
            let (meta, body) = match self.store.get(&self.site, file).await {
                Ok(found) => found,
                Err(FileError::NotFound { .. }) => return Ok(None),
                Err(_) => return Err(HostError::Store),
            };
            if meta.size > self.max_bytes {
                return Err(HostError::Invalid("the object is larger than the limit"));
            }
            let mut bytes = Vec::with_capacity(usize::try_from(meta.size).unwrap_or(0));
            let mut body = body.take(self.max_bytes.saturating_add(1));
            body.read_to_end(&mut bytes)
                .await
                .map_err(|_| HostError::Store)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > self.max_bytes {
                return Err(HostError::Invalid("the object is larger than the limit"));
            }
            Ok(Some(bytes))
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Vec<String>, HostError>> {
        Box::pin(async move {
            // Only whole directories are listed: `registry/`, `recordings/`.
            let dir = match prefix.strip_suffix('/') {
                Some(dir) => dir,
                None if prefix.is_empty() => "",
                None => return Err(HostError::Invalid("a listing names a whole directory")),
            };
            let mut stored = OBJECT_ROOT.to_owned();
            if !dir.is_empty() {
                // Encode through a throw-away leaf so the directory goes the same road as keys.
                let encoded = encode_key(&format!("{dir}/x"))?;
                stored = encoded.strip_suffix("/x").unwrap_or(OBJECT_ROOT).to_owned();
            }
            let wanted = FileKeyPrefix::new(&format!("{}/{stored}", Visibility::Private))
                .map_err(|_| HostError::Invalid("the directory is not storable"))?;
            let mut out = Vec::new();
            let mut after: Option<FileKey> = None;
            loop {
                let page = self
                    .store
                    .list(&self.site, &wanted, after.as_ref(), LIST_PAGE)
                    .await
                    .map_err(|_| HostError::Store)?;
                let Some(last) = page.last() else { break };
                after = Some(last.key.clone());
                let full = page.len() >= LIST_PAGE;
                for meta in &page {
                    if let Some(key) = decode_key(meta.key.rest()) {
                        out.push(key);
                    }
                }
                if !full {
                    break;
                }
            }
            Ok(out)
        })
    }
}

impl Retire for FileObjects {
    fn remove<'a>(&'a self, key: &'a str) -> BoxFuture<'a, std::result::Result<(), HostError>> {
        Box::pin(async move {
            let file = file_key(key)?;
            self.store
                .delete(&self.site, file)
                .await
                .map(|_| ())
                .map_err(|_| HostError::Store)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The sealer
// ---------------------------------------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    let raw = text.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    raw.chunks(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

/// The [`Sealer`] over the secrets service's vault. The vault seals text and is asynchronous, the
/// port is synchronous and byte-oriented, so a recording goes in as hex and the call waits for the
/// vault on the current multi-thread runtime. Anywhere else (no runtime, or a current-thread one)
/// the call is refused, never run unsealed.
pub struct VaultSealer {
    vault: Arc<dyn SecretVault>,
}

impl VaultSealer {
    /// A sealer over `vault`.
    pub fn new(vault: Arc<dyn SecretVault>) -> Self {
        Self { vault }
    }

    fn wait<T>(
        future: impl std::future::Future<Output = std::result::Result<T, spot_secrets::SecretError>>,
    ) -> std::result::Result<T, HostError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| HostError::Seal)?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return Err(HostError::Seal);
        }
        tokio::task::block_in_place(|| handle.block_on(future)).map_err(|_| HostError::Seal)
    }
}

impl Sealer for VaultSealer {
    fn seal(&self, context: &str, plain: &[u8]) -> std::result::Result<Vec<u8>, HostError> {
        let secret = SecretString::new(to_hex(plain));
        Self::wait(self.vault.seal(&secret, context.as_bytes()))
    }

    fn open(&self, context: &str, sealed: &[u8]) -> std::result::Result<Vec<u8>, HostError> {
        let secret = Self::wait(self.vault.open(sealed, context.as_bytes()))?;
        from_hex(secret.expose()).ok_or(HostError::Seal)
    }
}

// ---------------------------------------------------------------------------------------------
// The duty check
// ---------------------------------------------------------------------------------------------

/// A canonical hyphenated UUID, as the record id of a user (`None` for anything else).
fn parse_record_id(text: &str) -> Option<RecordId> {
    let raw = text.as_bytes();
    if raw.len() != 36 || ![8, 13, 18, 23].iter().all(|&i| raw[i] == b'-') {
        return None;
    }
    let digits: Vec<u8> = raw.iter().copied().filter(|&c| c != b'-').collect();
    let bytes = from_hex(std::str::from_utf8(&digits).ok()?)?;
    RecordId::from_bytes(<[u8; 16]>::try_from(bytes).ok()?).ok()
}

/// The [`Duties`] over the site's permission engine: a duty is an app permission of the same name
/// (`security_administrator`, `security_auditor`), decided for the user's principal against the
/// engine in force right now. An unknown user, an undeclared permission, a user id that is not a
/// record id and another tenant are all refusals; nothing is cached.
pub struct PermissionDuties {
    tenant: String,
    check: Arc<dyn AppPermissionCheck>,
    principals: Arc<dyn PrincipalSource>,
}

impl PermissionDuties {
    /// The duty check of `tenant`.
    pub fn new(
        tenant: &str,
        check: Arc<dyn AppPermissionCheck>,
        principals: Arc<dyn PrincipalSource>,
    ) -> Self {
        Self {
            tenant: tenant.to_owned(),
            check,
            principals,
        }
    }
}

impl Duties for PermissionDuties {
    fn holds<'a>(
        &'a self,
        tenant: &'a str,
        user: &'a str,
        duty: &'a str,
    ) -> BoxFuture<'a, std::result::Result<bool, HostError>> {
        Box::pin(async move {
            if tenant != self.tenant {
                return Err(HostError::Denied);
            }
            let Some(id) = parse_record_id(user) else {
                return Err(HostError::Denied);
            };
            let principal = self.principals.resolve(&id).await;
            Ok(self.check.holds(&principal, duty).await)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The bridge and the retention
// ---------------------------------------------------------------------------------------------

/// The agent bridge of a host that has no path to a host agent: every kill is refused with the
/// catalogued `break_glass_host.not_wired`. A revoke still closes the session first, and the
/// break-glass sweep keeps retrying the kill, so a real bridge provided later delivers it.
pub struct RefusingBridge;

impl AgentBridge for RefusingBridge {
    fn kill<'a>(
        &'a self,
        _tenant: &'a str,
        _session: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>> {
        Box::pin(async { Err(HostError::NotWired("agent bridge")) })
    }
}

/// The retention of recordings from this module's setting.
pub struct ConfiguredRetention {
    days: u32,
}

impl ConfiguredRetention {
    /// Keep recordings for `days` days.
    pub fn new(days: u32) -> Self {
        Self { days }
    }
}

impl Retention for ConfiguredRetention {
    fn recording_days(&self, _tenant: &str) -> Option<u32> {
        Some(self.days)
    }
}

// ---------------------------------------------------------------------------------------------
// The support-access caller (WP-IAM-12.T9)
// ---------------------------------------------------------------------------------------------

/// A boxed future of the callers below (the `spot-http` shape, not the break-glass one).
pub type ApiFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Who a request's credential names, as a person's session: the user and the session record.
pub trait SessionSource: Send + Sync {
    /// `None` when there is no credential, it does not resolve, it is an API key (no session),
    /// the session is restricted to choosing a password or the caller is not a person of the
    /// staff or partner audience. Every `None` is a refusal.
    fn session<'a>(&'a self, ctx: &'a RequestCtx) -> ApiFuture<'a, Option<(StorageId, StorageId)>>;
}

/// Whether a session's step-up marker is recent: the shared reader
/// (`services::step_up::session_is_current`) in production.
pub trait MarkerReader: Send + Sync {
    /// Whether the marker of `session` is within the step-up window now.
    ///
    /// # Errors
    ///
    /// When the record cannot be read: an error is a refusal.
    fn current<'a>(
        &'a self,
        session: StorageId,
    ) -> ApiFuture<'a, std::result::Result<bool, ApiError>>;
}

/// [`SessionSource`] over the site's sign-in service.
pub struct LiveSessions {
    site: String,
    live: Arc<SiteLive>,
}

impl LiveSessions {
    /// The sessions of `site`'s sign-in service.
    #[must_use]
    pub fn new(site: &str, live: Arc<SiteLive>) -> Self {
        Self {
            site: site.to_owned(),
            live,
        }
    }
}

impl SessionSource for LiveSessions {
    fn session<'a>(&'a self, ctx: &'a RequestCtx) -> ApiFuture<'a, Option<(StorageId, StorageId)>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.site {
                return None;
            }
            let auth = self.live.auth()?;
            let who = auth.authenticate_on(ctx, PathClass::Api).await.ok()?;
            // A person's own session: never an API key, never a session that has not chosen its
            // password, never another audience.
            let session = who
                .session_id
                .as_deref()
                .and_then(spot_http::api::codec::parse_record_id)?;
            if !matches!(who.via, spot_login::Via::Password)
                || !matches!(who.audience, Audience::Staff | Audience::Partner)
                || auth.is_restricted(&who)
            {
                return None;
            }
            Some((who.user_id, session))
        })
    }
}

/// [`MarkerReader`] over the site's database and the injected clock.
pub struct PgMarkers {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    clock: Arc<dyn Clock>,
    window_secs: i64,
}

impl PgMarkers {
    /// The marker reader of one site, judging "recent" by `window_secs` on `clock`.
    #[must_use]
    pub fn new(
        backend: PgBackend,
        catalog: Arc<Catalog>,
        clock: Arc<dyn Clock>,
        window_secs: i64,
    ) -> Self {
        Self {
            backend,
            catalog,
            clock,
            window_secs,
        }
    }
}

impl MarkerReader for PgMarkers {
    fn current<'a>(
        &'a self,
        session: StorageId,
    ) -> ApiFuture<'a, std::result::Result<bool, ApiError>> {
        Box::pin(async move {
            session_is_current(
                &self.backend,
                &self.catalog,
                session,
                self.clock.now_utc(),
                self.window_secs,
            )
            .await
        })
    }
}

/// The [`SupportCaller`] of a site: the user is the one the credential's own session names (never
/// a system principal), `is_security_admin` is the `security_administrator` duty decided by the
/// permission engine for that user, and `step_up_current` is the session's marker read through the
/// shared reader. An engine or marker error, and every credential that is not a person's session,
/// is "no caller": a refusal, never an allow.
pub struct PermissionCaller {
    tenant: String,
    sessions: Arc<dyn SessionSource>,
    markers: Arc<dyn MarkerReader>,
    duties: Arc<dyn Duties>,
}

impl PermissionCaller {
    /// The caller resolver of `tenant`.
    #[must_use]
    pub fn new(
        tenant: &str,
        sessions: Arc<dyn SessionSource>,
        markers: Arc<dyn MarkerReader>,
        duties: Arc<dyn Duties>,
    ) -> Self {
        Self {
            tenant: tenant.to_owned(),
            sessions,
            markers,
            duties,
        }
    }
}

impl SupportCaller for PermissionCaller {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> ApiFuture<'a, Option<CallerInfo>> {
        Box::pin(async move {
            let (user, session) = self.sessions.session(ctx).await?;
            let user_id = user.to_string();
            let is_security_admin = self
                .duties
                .holds(&self.tenant, &user_id, ADMIN_DUTY)
                .await
                .ok()?;
            let step_up_current = self.markers.current(session).await.ok()?;
            Some(CallerInfo {
                user_id,
                is_security_admin,
                step_up_current,
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Assembling the parts
// ---------------------------------------------------------------------------------------------

/// The real components this site has, as the wiring finds them. A part that is `None` is missing.
#[derive(Default)]
pub struct Sources {
    /// The tenant (the site name).
    pub tenant: String,
    /// The break-glass chain, when the audit store is prepared and protected.
    pub log: Option<Arc<dyn EventLog>>,
    /// The site's file store.
    pub files: Option<Arc<dyn FileStore>>,
    /// The secrets vault.
    pub vault: Option<Arc<dyn SecretVault>>,
    /// The permission check and the principal source.
    pub permissions: Option<(Arc<dyn AppPermissionCheck>, Arc<dyn PrincipalSource>)>,
}

/// What the wiring did, for the check and the operator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The parts provided from a real component.
    pub real: Vec<&'static str>,
    /// The parts provided as a refusal (a catalogued error on every call).
    pub refusing: Vec<&'static str>,
    /// The required parts nothing provided, which keep break-glass closed.
    pub missing: Vec<&'static str>,
}

/// Builds the break-glass parts from what the site has. The required four are present or named
/// in [`Report::missing`]; the bridge is always present, as a refusal until a real one exists.
#[must_use]
pub fn build_parts(sources: Sources, settings: &Settings) -> (Parts, Report) {
    let mut parts = Parts::default();
    let mut report = Report::default();

    match sources.log {
        Some(log) => {
            parts.log = Some(log);
            report.real.push("audit chain");
        }
        None => report.missing.push("audit chain"),
    }

    match (sources.files, SiteId::new(&sources.tenant)) {
        (Some(store), Ok(site)) => {
            let objects = Arc::new(FileObjects::new(store, site, settings.max_object_bytes));
            parts.store = Some(Arc::clone(&objects) as Arc<dyn ObjectStore>);
            parts.retire = Some(objects as Arc<dyn Retire>);
            report.real.push("object store");
        }
        _ => report.missing.push("object store"),
    }

    match sources.vault {
        Some(vault) => {
            parts.sealer = Some(Arc::new(VaultSealer::new(vault)));
            report.real.push("sealer");
        }
        None => report.missing.push("sealer"),
    }

    match sources.permissions {
        Some((check, principals)) => {
            parts.duties = Some(Arc::new(PermissionDuties::new(
                &sources.tenant,
                check,
                principals,
            )));
            report.real.push("duty check");
        }
        None => report.missing.push("duty check"),
    }

    parts.bridge = Some(Arc::new(RefusingBridge));
    report.refusing.push("agent bridge");

    if let Some(days) = settings.retention_days {
        parts.retention = Some(Arc::new(ConfiguredRetention::new(days)));
        report.real.push("retention");
    }
    (parts, report)
}

// ---------------------------------------------------------------------------------------------
// The service and the registration
// ---------------------------------------------------------------------------------------------

struct WiringService {
    report: Report,
}

impl Service for WiringService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if self.report.missing.is_empty() {
                return Ok(());
            }
            Err(format!(
                "{}: no break-glass session can be authorised: missing {}",
                code::PART_MISSING,
                self.report.missing.join(", ")
            ))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

/// What the context holds that the wiring reads: owned, so nothing borrowed from the context is
/// held across an await.
struct Found {
    sources: Sources,
    pool: Option<deadpool_postgres::Pool>,
}

/// The components of the site this module can find. Nothing but the vault when the context
/// carries no site (a context built for a test).
fn collect(ctx: &SiteCtx<'_>) -> Found {
    let mut sources = Sources {
        tenant: ctx.label().to_owned(),
        ..Sources::default()
    };
    sources.vault = ctx.port::<SecretsHost>().map(|host| host.vault());
    let Ok(env) = ctx.env().copied() else {
        return Found {
            sources,
            pool: None,
        };
    };
    sources.files = env.live.files().map(|files| files.store());
    sources.permissions = Some((
        Arc::new(SnapshotAppPermissions::new(Arc::clone(
            &env.permission.authorizer,
        ))) as Arc<dyn AppPermissionCheck>,
        Arc::clone(&env.permission.principals) as Arc<dyn PrincipalSource>,
    ));
    Found {
        sources,
        pool: Some(env.backend.pool()),
    }
}

/// Gives the sources the chain, but only when the database says its guard against rewrites is
/// complete: an unprotected chain is not a chain the customer can rely on.
async fn with_chain(site: &str, found: Found) -> Sources {
    let Found {
        mut sources, pool, ..
    } = found;
    let Some(pool) = pool else { return sources };
    match spot_audit_store::verify(&pool).await {
        Ok(protection) if protection.is_complete() => {
            sources.log = Some(Arc::new(ChainEventLog::new(
                PgAuditStore::new(pool),
                &scope_of(site),
            )));
        }
        Ok(protection) => {
            tracing::warn!(
                site,
                missing = %protection.missing().join(", "),
                "break-glass: the audit store is not protected, so no chain is provided"
            );
        }
        Err(_) => {
            tracing::warn!(
                site,
                "break-glass: the audit store could not be checked, so no chain is provided"
            );
        }
    }
    sources
}

fn provide_parts(ctx: &mut SiteCtx<'_>, parts: &Parts) -> Result<()> {
    if let Some(port) = &parts.log {
        ctx.provide::<dyn EventLog>(Arc::clone(port))?;
    }
    if let Some(port) = &parts.store {
        ctx.provide::<dyn ObjectStore>(Arc::clone(port))?;
    }
    if let Some(port) = &parts.retire {
        ctx.provide::<dyn Retire>(Arc::clone(port))?;
    }
    if let Some(port) = &parts.sealer {
        ctx.provide::<dyn Sealer>(Arc::clone(port))?;
    }
    if let Some(port) = &parts.duties {
        ctx.provide::<dyn Duties>(Arc::clone(port))?;
    }
    if let Some(port) = &parts.bridge {
        ctx.provide::<dyn AgentBridge>(Arc::clone(port))?;
    }
    if let Some(port) = &parts.retention {
        ctx.provide::<dyn Retention>(Arc::clone(port))?;
    }
    Ok(())
}

/// Provides `dyn SupportCaller` (read by `services/support_access.rs`, which starts later) when
/// the site has its sign-in service and a duty check. A context built without a site, or a site
/// without a permission engine, provides none, and the support-access service then has no caller:
/// every operation is refused.
fn provide_caller(ctx: &mut SiteCtx<'_>, parts: &Parts) -> Result<()> {
    let Some(duties) = parts.duties.as_ref() else {
        return Ok(());
    };
    let Some(env) = ctx.env().ok().copied() else {
        return Ok(());
    };
    let window = crate::services::identity_admin::step_up_window_secs(ctx)?;
    let tenant = env.site.site.clone();
    let sessions = Arc::new(LiveSessions::new(&tenant, Arc::clone(env.live)));
    let markers = Arc::new(PgMarkers::new(
        env.backend.clone(),
        env.permission.catalog.clone(),
        Arc::clone(ctx.clock()),
        window,
    ));
    let caller: Arc<dyn SupportCaller> = Arc::new(PermissionCaller::new(
        &tenant,
        sessions,
        markers,
        Arc::clone(duties),
    ));
    ctx.provide::<dyn SupportCaller>(caller)
}

/// Registers the wiring: provides the ports of [`Parts`] and adds the check.
///
/// # Errors
///
/// A setting that does not decode or is out of bounds, or a port some other module already
/// provides (two providers of one port is an error naming both modules).
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    let found = collect(ctx);
    let site = ctx.label().to_owned();
    let sources = with_chain(&site, found).await;
    let (parts, report) = build_parts(sources, &settings);
    provide_parts(ctx, &parts)?;
    provide_caller(ctx, &parts)?;
    if !report.refusing.is_empty() {
        tracing::info!(
            site = ctx.label(),
            refusing = %report.refusing.join(", "),
            "break-glass: parts without a real component refuse every call"
        );
    }
    ctx.add(Arc::new(WiringService { report }));
    Ok(())
}
