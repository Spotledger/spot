//! The management API, hosted (WP-ADM-01.T5, ADR-44, ADR-097). Operator guide:
//! `docs/running-spot/mgmt_api.md`.
//!
//! `spot-mgmt-api` (WP-ADM-01.T4) is the library: the router of `contracts/management-api`, mutual
//! TLS, the signed-command intake and the command lifecycle. This module hosts it as an **optional**
//! service on its **own listener** (own port, own TLS), never on the public API port and never
//! sharing the public router or its middleware (no cookies, no CORS).
//!
//! # Settings
//!
//! `[services.mgmt_api]`: `enabled` (default **false**), `listen` (default `127.0.0.1:9443`; a
//! non-loopback address needs `allow_remote = true` and is logged at start), `server_cert`,
//! `server_key` (the key file must be mode 0600 or 0640 and owned by the service user), `operator_ca`,
//! `operators` (the certificate subjects on the allow-list), `administrator_keys` (a file of Ed25519
//! public keys, 64 hex characters per line, read at start and on reload) and `audience` (default: the
//! site id). `store` is `postgres` (default); `memory` is refused unless `dev = true` is also set.
//!
//! # What it builds
//!
//! - the PostgreSQL nonce store (`spot-signed-command-pg`, fail closed, so a restart cannot reopen the
//!   replay window) and a PostgreSQL command store (`spot_control.mgmt_command`);
//! - the operations other modules contribute through the [`MgmtHandlers`] port (an aggregator: the
//!   modules with a greater `ORDER` add their operation and handler during their own `register`; this
//!   module seals the set in a finaliser, as the conditions service does);
//! - an audit sink into the site's identity audit chain: every request that reaches authentication
//!   leaves a record, and a record that cannot be stored fails the command (fail closed).
//!
//! # Level and failure
//!
//! When enabled the service is **required**: a half-working administrative plane is worse than none.
//! The start-up check parses the certificate, key and CA, checks the key file, reaches the nonce table
//! and checks the clock; any failure stops the site. `spot reload` and SIGHUP re-read the certificate
//! files, the operator CA, the operator list and the administrator keys without dropping commands that
//! are running: new connections get the new configuration, open ones keep theirs. Shutdown waits for
//! running commands up to `drain_seconds`.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use deadpool_postgres::Pool;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use serde_json::{Value, json};
use spot_core::Clock;
use spot_http::api::service::{Door, DoorTier, OfflineRefusal};
use spot_mgmt_api::handlers::{
    Handler, HandlerContext, HandlerError, HandlerRegistry, HandlerRequest, HandlerResult,
};
use spot_mgmt_api::lifecycle::{
    CommandRecord, CommandState, CommandStore, ProgressLine, StoreError,
};
use spot_mgmt_api::ports::{
    ApprovalCheck, AuditError, AuditOutcome, AuditRecord as MgmtAudit, AuditSink, BoxFuture,
    NoApprovals, OperatorAllowList, OperatorEntry,
};
use spot_mgmt_api::tls::{HandshakeLimiter, ReloadableTls, serve_reloadable, tls_config};
use spot_mgmt_api::{Deps, MAX_CLOCK_SKEW_SECS, router};
use spot_mgmt_ops::{Catalogue, OperationSpec};
use spot_signed_command::NonceStore;
use spot_signed_command_pg::PgNonceStore;
use tokio::sync::watch;
use tower::ServiceExt as _;

use crate::services::realtime::{OfflineSlot, door_slot, finalise_door};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: early among the hosting modules (1000, the registry's floor), so the modules
/// that offer operations (greater `ORDER`) can add to the [`MgmtHandlers`] port during their own
/// `register`.
pub const ORDER: u32 = 1000;

// ---- settings --------------------------------------------------------------------------------

/// `[services.mgmt_api]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service runs. Off by default: the port stays closed.
    pub enabled: bool,
    /// The listen address.
    pub listen: String,
    /// Required for a non-loopback address.
    pub allow_remote: bool,
    /// The server certificate chain (PEM).
    pub server_cert: Option<PathBuf>,
    /// The server key (PEM; mode 0600 or 0640, owned by the service user).
    pub server_key: Option<PathBuf>,
    /// The operator CA (PEM).
    pub operator_ca: Option<PathBuf>,
    /// The certificate subjects on the operator allow-list (RFC 2253 text, e.g. `CN=alice`).
    pub operators: Vec<String>,
    /// A file of administrator public keys, one 64-hex-character Ed25519 key per line.
    pub administrator_keys: Option<PathBuf>,
    /// The audience of signed commands; defaults to the site id.
    pub audience: Option<String>,
    /// `postgres` (default) or `memory` (development only, with `dev = true`).
    pub store: String,
    /// Allows the memory nonce store.
    pub dev: bool,
    /// How long shutdown waits for running commands.
    pub drain_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:9443".to_owned(),
            allow_remote: false,
            server_cert: None,
            server_key: None,
            operator_ca: None,
            operators: Vec::new(),
            administrator_keys: None,
            audience: None,
            store: "postgres".to_owned(),
            dev: false,
            drain_seconds: 30,
        }
    }
}

impl Settings {
    /// Checks the settings of an enabled service.
    ///
    /// # Errors
    /// A message naming the setting (never a value that may be a secret).
    pub fn validate(&self) -> std::result::Result<(), String> {
        let address: SocketAddr = self
            .listen
            .parse()
            .map_err(|_| "listen is not an address like 127.0.0.1:9443".to_owned())?;
        if !address.ip().is_loopback() && !self.allow_remote {
            return Err("a non-loopback listen address needs allow_remote = true".to_owned());
        }
        for (name, present) in [
            ("server_cert", self.server_cert.is_some()),
            ("server_key", self.server_key.is_some()),
            ("operator_ca", self.operator_ca.is_some()),
            ("administrator_keys", self.administrator_keys.is_some()),
        ] {
            if !present {
                return Err(format!("{name} is required"));
            }
        }
        if self.operators.is_empty() {
            return Err("operators must list at least one certificate subject".to_owned());
        }
        match self.store.as_str() {
            "postgres" => {}
            "memory" if self.dev => {}
            "memory" => {
                return Err(
                    "store = \"memory\" is refused outside the development profile (dev = true)"
                        .to_owned(),
                );
            }
            _ => return Err("store is postgres or memory".to_owned()),
        }
        if !(1..=3600).contains(&self.drain_seconds) {
            return Err("drain_seconds is 1 to 3600".to_owned());
        }
        Ok(())
    }
}

// ---- the handlers port -------------------------------------------------------------------------

struct Entry {
    module: &'static str,
    spec: OperationSpec,
    handler: Arc<dyn Handler>,
    cancellable: bool,
}

/// The aggregator modules add their management operations to. Provided by this module at `ORDER`
/// 100; a module with a greater `ORDER` calls `ctx.port::<MgmtHandlers>()` and [`add`](Self::add).
#[derive(Default)]
pub struct MgmtHandlers {
    entries: Mutex<Vec<Entry>>,
}

impl MgmtHandlers {
    /// Adds an operation and its handler.
    pub fn add(
        &self,
        module: &'static str,
        spec: OperationSpec,
        handler: Arc<dyn Handler>,
        cancellable: bool,
    ) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Entry {
                module,
                spec,
                handler,
                cancellable,
            });
    }

    fn take(&self) -> Vec<Entry> {
        std::mem::take(&mut *self.entries.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

// ---- metrics -------------------------------------------------------------------------------------

/// Counters of the management plane: commands by operation and outcome, signature and replay
/// refusals, and refused handshakes.
#[derive(Default)]
pub struct MgmtMetrics {
    commands: Mutex<BTreeMap<(String, String), u64>>,
    signature_refusals: AtomicU64,
    replay_refusals: AtomicU64,
    handshake_failures: AtomicU64,
}

impl MgmtMetrics {
    fn observe(&self, record: &MgmtAudit) {
        let outcome = match record.outcome {
            AuditOutcome::Allowed => return,
            AuditOutcome::Refused => "refused",
            AuditOutcome::Succeeded => "succeeded",
            AuditOutcome::Failed => "failed",
            AuditOutcome::Cancelled => "cancelled",
        };
        if let Some(operation) = &record.operation {
            let mut map = self.commands.lock().unwrap_or_else(|p| p.into_inner());
            *map.entry((operation.clone(), outcome.to_owned()))
                .or_insert(0) += 1;
        }
        match record.reason.as_deref() {
            Some(
                "mgmt.signature_invalid" | "mgmt.signature_expired" | "mgmt.signature_unbound",
            ) => {
                self.signature_refusals.fetch_add(1, Ordering::Relaxed);
            }
            Some("mgmt.replayed") => {
                self.replay_refusals.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Commands counted by `(operation, outcome)`.
    #[must_use]
    pub fn commands_total(&self) -> BTreeMap<(String, String), u64> {
        self.commands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Commands refused for their signature (invalid, expired).
    #[must_use]
    pub fn signature_refusals(&self) -> u64 {
        self.signature_refusals.load(Ordering::Relaxed)
    }

    /// Commands refused as replays.
    #[must_use]
    pub fn replay_refusals(&self) -> u64 {
        self.replay_refusals.load(Ordering::Relaxed)
    }

    /// Connection attempts refused by the per-address handshake limit.
    #[must_use]
    pub fn handshake_failures(&self) -> u64 {
        self.handshake_failures.load(Ordering::Relaxed)
    }
}

// ---- audit into the identity chain -------------------------------------------------------------

/// The audit sink over the site's identity audit chain, counting what it sees.
struct ChainSink {
    recorder: crate::audit::Recorder,
    metrics: Arc<MgmtMetrics>,
    counter: AtomicU64,
}

impl AuditSink for ChainSink {
    fn record(&self, record: MgmtAudit) -> BoxFuture<'_, std::result::Result<String, AuditError>> {
        self.metrics.observe(&record);
        let mut entry = self
            .recorder
            .record(&format!("mgmt_{}", record.action.replace(' ', "_")));
        entry.user = record.principal.clone();
        entry.source = record
            .certificate_fingerprint
            .as_ref()
            .map(|f| f.chars().take(16).collect());
        entry.session = record.command_id.clone();
        entry.path = record.operation.clone();
        entry.reason = Some(match record.reason.as_deref() {
            Some(reason) => reason.to_owned(),
            None => format!("{:?}", record.outcome).to_lowercase(),
        });
        let sequence = self.counter.fetch_add(1, Ordering::Relaxed);
        let result = self
            .recorder
            .required(&entry)
            .map(|()| format!("mgmt-audit-{sequence}"))
            .map_err(|_| AuditError);
        Box::pin(std::future::ready(result))
    }
}

// ---- the PostgreSQL command store ------------------------------------------------------------

/// The command store over the site's database: `spot_control.mgmt_command`.
pub struct PgCommandStore {
    pool: Pool,
    retention_secs: i64,
}

const COMMAND_DDL: &[&str] = &[
    "CREATE SCHEMA IF NOT EXISTS spot_control",
    "CREATE TABLE IF NOT EXISTS spot_control.mgmt_command (\
        id text PRIMARY KEY, operation text NOT NULL, principal text NOT NULL, state text NOT NULL, \
        progress text NOT NULL DEFAULT '[]', result text, error_code text, audit_ref text NOT NULL DEFAULT '', \
        created_at bigint NOT NULL, finished_at bigint, idempotency_key text NOT NULL, \
        body_digest text NOT NULL, cancellable boolean NOT NULL, \
        UNIQUE (principal, idempotency_key))",
];

const COLUMNS: &str = "id, operation, principal, state, progress, result, error_code, audit_ref, \
    created_at, finished_at, idempotency_key, body_digest, cancellable";

fn unavailable<E>(_: E) -> StoreError {
    StoreError::Unavailable
}

fn state_of(text: &str) -> CommandState {
    match text {
        "running" => CommandState::Running,
        "succeeded" => CommandState::Succeeded,
        "failed" => CommandState::Failed,
        "cancelled" => CommandState::Cancelled,
        _ => CommandState::Accepted,
    }
}

fn progress_to_text(lines: &[ProgressLine]) -> String {
    json!(
        lines
            .iter()
            .map(|l| json!([l.step, l.percent, l.at]))
            .collect::<Vec<_>>()
    )
    .to_string()
}

fn progress_from_text(text: &str) -> Vec<ProgressLine> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(ProgressLine {
                step: item.get(0)?.as_str()?.to_owned(),
                percent: u8::try_from(item.get(1)?.as_u64()?).ok()?,
                at: item.get(2)?.as_i64()?,
            })
        })
        .collect()
}

fn record_of(row: &tokio_postgres::Row) -> CommandRecord {
    let state: String = row.get(3);
    let progress: String = row.get(4);
    let result: Option<String> = row.get(5);
    CommandRecord {
        id: row.get(0),
        operation: row.get(1),
        principal: row.get(2),
        state: state_of(&state),
        progress: progress_from_text(&progress),
        result: result.and_then(|text| serde_json::from_str(&text).ok()),
        error_code: row.get(6),
        audit_ref: row.get(7),
        created_at: row.get(8),
        finished_at: row.get(9),
        idempotency_key: row.get(10),
        body_digest: row.get(11),
        cancellable: row.get(12),
    }
}

impl PgCommandStore {
    /// A store on `pool` keeping finished commands for seven days.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            retention_secs: spot_mgmt_api::lifecycle::DEFAULT_RETENTION_SECS,
        }
    }

    /// Creates the table (idempotent).
    ///
    /// # Errors
    /// [`StoreError::Unavailable`].
    pub async fn prepare(&self) -> std::result::Result<(), StoreError> {
        let client = self.pool.get().await.map_err(unavailable)?;
        for statement in COMMAND_DDL {
            client.batch_execute(statement).await.map_err(unavailable)?;
        }
        Ok(())
    }
}

impl CommandStore for PgCommandStore {
    fn insert(
        &self,
        record: CommandRecord,
        now: i64,
    ) -> BoxFuture<'_, std::result::Result<(), StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(unavailable)?;
            let cutoff = now - self.retention_secs;
            client
                .execute(
                    "DELETE FROM spot_control.mgmt_command WHERE finished_at IS NOT NULL AND finished_at < $1",
                    &[&cutoff],
                )
                .await
                .map_err(unavailable)?;
            let result = client
                .execute(
                    "INSERT INTO spot_control.mgmt_command (id, operation, principal, state, created_at, \
                     idempotency_key, body_digest, cancellable) VALUES ($1, $2, $3, 'queued', $4, $5, $6, $7)",
                    &[&record.id, &record.operation, &record.principal, &record.created_at,
                      &record.idempotency_key, &record.body_digest, &record.cancellable],
                )
                .await;
            match result {
                Ok(_) => Ok(()),
                Err(error)
                    if error.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) =>
                {
                    Err(StoreError::Duplicate)
                }
                Err(_) => Err(StoreError::Unavailable),
            }
        })
    }

    fn get<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<CommandRecord>, StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(unavailable)?;
            let sql = format!("SELECT {COLUMNS} FROM spot_control.mgmt_command WHERE id = $1");
            let row = client
                .query_opt(sql.as_str(), &[&id])
                .await
                .map_err(unavailable)?;
            Ok(row.as_ref().map(record_of))
        })
    }

    fn find_by_idempotency<'a>(
        &'a self,
        principal: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<CommandRecord>, StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(unavailable)?;
            let sql = format!(
                "SELECT {COLUMNS} FROM spot_control.mgmt_command WHERE principal = $1 AND idempotency_key = $2"
            );
            let row = client
                .query_opt(sql.as_str(), &[&principal, &key])
                .await
                .map_err(unavailable)?;
            Ok(row.as_ref().map(record_of))
        })
    }

    fn start<'a>(
        &'a self,
        id: &'a str,
        audit_ref: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(unavailable)?;
            client
                .execute(
                    "UPDATE spot_control.mgmt_command SET audit_ref = $2, \
                     state = CASE WHEN state = 'queued' THEN 'running' ELSE state END WHERE id = $1",
                    &[&id, &audit_ref],
                )
                .await
                .map_err(unavailable)?;
            Ok(())
        })
    }

    fn progress<'a>(
        &'a self,
        id: &'a str,
        line: ProgressLine,
    ) -> BoxFuture<'a, std::result::Result<(), StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(unavailable)?;
            let row = client
                .query_opt(
                    "SELECT progress FROM spot_control.mgmt_command \
                     WHERE id = $1 AND state IN ('queued', 'running')",
                    &[&id],
                )
                .await
                .map_err(unavailable)?;
            let Some(row) = row else { return Ok(()) };
            let text: String = row.get(0);
            let mut lines = progress_from_text(&text);
            lines.push(line);
            let excess = lines
                .len()
                .saturating_sub(spot_mgmt_api::lifecycle::MAX_PROGRESS_LINES);
            lines.drain(..excess);
            client
                .execute(
                    "UPDATE spot_control.mgmt_command SET progress = $2 WHERE id = $1",
                    &[&id, &progress_to_text(&lines)],
                )
                .await
                .map_err(unavailable)?;
            Ok(())
        })
    }

    fn finish<'a>(
        &'a self,
        id: &'a str,
        state: CommandState,
        result: Option<Value>,
        error_code: Option<String>,
        now: i64,
    ) -> BoxFuture<'a, std::result::Result<(), StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(unavailable)?;
            let result_text = result.map(|value| value.to_string());
            client
                .execute(
                    "UPDATE spot_control.mgmt_command SET state = $2, result = $3, error_code = $4, \
                     finished_at = $5 WHERE id = $1 AND state IN ('queued', 'running')",
                    &[&id, &state.wire(), &result_text, &error_code, &now],
                )
                .await
                .map_err(unavailable)?;
            Ok(())
        })
    }

    fn purge(&self, older_than: i64) -> BoxFuture<'_, std::result::Result<usize, StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(unavailable)?;
            let removed = client
                .execute(
                    "DELETE FROM spot_control.mgmt_command WHERE finished_at IS NOT NULL AND finished_at < $1",
                    &[&older_than],
                )
                .await
                .map_err(unavailable)?;
            Ok(usize::try_from(removed).unwrap_or(usize::MAX))
        })
    }
}

// ---- files -----------------------------------------------------------------------------------

/// The key file must be mode 0600 or 0640 and owned by the service user (Linux; elsewhere only the
/// mode is checked on Unix).
fn check_key_file(path: &Path) -> std::result::Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let meta = std::fs::metadata(path).map_err(|_| "server_key cannot be read".to_owned())?;
        if meta.permissions().mode() & 0o777 & !0o640 != 0 {
            return Err("server_key must be mode 0600 or 0640".to_owned());
        }
        #[cfg(target_os = "linux")]
        if let Ok(process) = std::fs::metadata("/proc/self")
            && process.uid() != meta.uid()
        {
            return Err("server_key must be owned by the service user".to_owned());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn certs(path: &Path, what: &str) -> std::result::Result<Vec<CertificateDer<'static>>, String> {
    let iter = CertificateDer::pem_file_iter(path).map_err(|_| format!("{what} cannot be read"))?;
    let list: std::result::Result<Vec<_>, _> = iter.collect();
    let list = list.map_err(|_| format!("{what} is not valid PEM"))?;
    if list.is_empty() {
        return Err(format!("{what} holds no certificate"));
    }
    Ok(list)
}

/// Loads the TLS configuration and the CA bundle.
fn load_tls(
    settings: &Settings,
) -> std::result::Result<(rustls::ServerConfig, Vec<CertificateDer<'static>>), String> {
    let (Some(cert), Some(key), Some(ca)) = (
        &settings.server_cert,
        &settings.server_key,
        &settings.operator_ca,
    ) else {
        return Err("server_cert, server_key and operator_ca are required".to_owned());
    };
    check_key_file(key)?;
    let chain = certs(cert, "server_cert")?;
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|_| "server_key is not a valid PEM key".to_owned())?;
    let ca = certs(ca, "operator_ca")?;
    let config = tls_config(chain, key, &ca).map_err(|e| e.to_string())?;
    Ok((config, ca))
}

fn load_admin_keys(path: &Path) -> std::result::Result<Vec<[u8; 32]>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| "administrator_keys cannot be read".to_owned())?;
    let mut keys = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let bytes = line.as_bytes();
        let mut key = [0u8; 32];
        if bytes.len() != 64 {
            return Err("administrator_keys holds a line that is not 64 hex characters".to_owned());
        }
        for (index, pair) in bytes.chunks(2).enumerate() {
            let text = std::str::from_utf8(pair)
                .map_err(|_| "administrator_keys is not text".to_owned())?;
            *key.get_mut(index)
                .ok_or("administrator_keys is malformed")? = u8::from_str_radix(text, 16)
                .map_err(|_| "administrator_keys holds a non-hex character".to_owned())?;
        }
        keys.push(key);
    }
    if keys.is_empty() {
        return Err("administrator_keys holds no key".to_owned());
    }
    Ok(keys)
}

/// The principal name of a certificate subject: the common name, lower-cased, reduced to
/// `[a-z0-9_.-]`.
fn principal_of(subject: &str) -> String {
    let name = subject
        .split(',')
        .find_map(|part| part.trim().strip_prefix("CN="))
        .unwrap_or(subject);
    name.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---- the running parts -----------------------------------------------------------------------

/// The error code a refused command ends with: fixed strings, never a principal or a parameter.
fn offline_code(refusal: OfflineRefusal) -> &'static str {
    match refusal {
        OfflineRefusal::Denied => "permission.denied",
        OfflineRefusal::NotReady => "server.unavailable",
    }
}

/// Counts the commands that are running, so shutdown can wait for them, and refuses a command
/// while the node holds no valid lease (WP-IAM-09.T5: the caller is an operator certificate, not a
/// person of the offline directory, so the decision is the node's own lease at tier one).
struct Counted {
    inner: Arc<dyn Handler>,
    active: Arc<AtomicUsize>,
    offline: Arc<OfflineSlot>,
}

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Handler for Counted {
    fn call(
        &self,
        context: HandlerContext,
        request: HandlerRequest,
    ) -> BoxFuture<'static, HandlerResult> {
        if let Err(refusal) = self.offline.admit_node(Door::Management, DoorTier::One) {
            return Box::pin(async move { Err(HandlerError::failed(offline_code(refusal))) });
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        let guard = ActiveGuard(Arc::clone(&self.active));
        let future = self.inner.call(context, request);
        Box::pin(async move {
            let _guard = guard;
            future.await
        })
    }
}

/// Everything a (re)build of the router needs that does not change on reload.
struct Parts {
    catalogue: Arc<Catalogue>,
    handlers: HandlerRegistry,
    nonces: Arc<dyn NonceStore>,
    approvals: Arc<dyn ApprovalCheck>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditSink>,
    commands: Arc<dyn CommandStore>,
    site_id: String,
}

/// The live service state, shared between the service and a reload.
pub struct Runtime {
    parts: Parts,
    tls: Arc<ReloadableTls>,
    router: Arc<RwLock<Router>>,
    active: Arc<AtomicUsize>,
}

fn operators_of(settings: &Settings) -> OperatorAllowList {
    OperatorAllowList::new(
        settings
            .operators
            .iter()
            .map(|subject| OperatorEntry::new(subject, &principal_of(subject)))
            .collect(),
    )
}

impl Runtime {
    fn build_router(
        &self,
        settings: &Settings,
        keys: Vec<[u8; 32]>,
        ca: Vec<CertificateDer<'static>>,
    ) -> Router {
        router(Deps {
            catalogue: Arc::clone(&self.parts.catalogue),
            handlers: self.parts.handlers.clone(),
            nonces: Arc::clone(&self.parts.nonces),
            administrator_keys: keys,
            operator_ca: ca,
            operators: operators_of(settings),
            approvals: Arc::clone(&self.parts.approvals),
            clock: Arc::clone(&self.parts.clock),
            audit: Arc::clone(&self.parts.audit),
            commands: Arc::clone(&self.parts.commands),
            site_id: settings
                .audience
                .clone()
                .unwrap_or_else(|| self.parts.site_id.clone()),
        })
    }

    /// Re-reads the certificate, key, operator CA, operator list and administrator keys. New
    /// connections get them; open connections and running commands are untouched. A failure keeps
    /// the old configuration.
    ///
    /// # Errors
    /// A message naming the file or setting that did not load.
    pub fn reload(&self, settings: &Settings) -> std::result::Result<(), String> {
        let (config, ca) = load_tls(settings)?;
        let keys = settings
            .administrator_keys
            .as_deref()
            .ok_or("administrator_keys is required")
            .map_err(str::to_owned)
            .and_then(load_admin_keys)?;
        let fresh = self.build_router(settings, keys, ca);
        self.tls.replace(config);
        if let Ok(mut current) = self.router.write() {
            *current = fresh;
        }
        Ok(())
    }
}

/// Serves whatever router is current.
#[derive(Clone)]
struct Swappable(Arc<RwLock<Router>>);

impl tower::Service<Request<Body>> for Swappable {
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<Response<Body>, std::convert::Infallible>,
                > + Send,
        >,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let current = self.0.read().map(|router| router.clone()).ok();
        Box::pin(async move {
            match current {
                Some(router) => router.oneshot(request).await,
                None => {
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    Ok(response)
                }
            }
        })
    }
}

struct MgmtApiService {
    runtime: Arc<Runtime>,
    settings: Settings,
    nonce_store: Option<Arc<PgNonceStore>>,
    command_store: Option<Arc<PgCommandStore>>,
    /// Held so the counters outlive the service; the handles read them through the port.
    #[allow(dead_code)]
    metrics: Arc<MgmtMetrics>,
    clock: Arc<dyn Clock>,
}

impl Service for MgmtApiService {
    fn name(&self) -> &'static str {
        "mgmt_api"
    }

    fn level(&self) -> Level {
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            load_tls(&self.settings)?;
            if let Some(path) = &self.settings.administrator_keys {
                load_admin_keys(path)?;
            }
            if let Some(store) = &self.nonce_store {
                store
                    .prepare()
                    .await
                    .map_err(|_| "the nonce table is not reachable".to_owned())?;
            }
            if let Some(store) = &self.command_store {
                store
                    .prepare()
                    .await
                    .map_err(|_| "the command table is not reachable".to_owned())?;
            }
            if self.clock.now_utc().unix_timestamp() < 1_767_225_600 {
                return Err("the system clock is before 2026".to_owned());
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let address: SocketAddr = self
                .settings
                .listen
                .parse()
                .map_err(|_| "listen is not an address".to_owned())?;
            if !address.ip().is_loopback() {
                tracing::warn!(%address, "the management API listens on a non-loopback address");
            }
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .map_err(|_| "the management listener cannot be bound".to_owned())?;
            let service =
                Router::new().fallback_service(Swappable(Arc::clone(&self.runtime.router)));
            let hup = {
                let runtime = Arc::clone(&self.runtime);
                let settings = self.settings.clone();
                let mut stop = stop.clone();
                tokio::spawn(async move { reload_on_hangup(runtime, settings, &mut stop).await })
            };
            let mut stopper = stop.clone();
            let limiter = Arc::new(HandshakeLimiter::standard());
            let served = serve_reloadable(
                listener,
                Arc::clone(&self.runtime.tls),
                service,
                Arc::clone(&self.clock),
                limiter,
                async move {
                    let _stopped = stopper.wait_for(|stop| *stop).await.is_ok();
                },
            )
            .await;
            hup.abort();
            served.map_err(|_| "the management listener stopped".to_owned())
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            let deadline = Duration::from_secs(self.settings.drain_seconds);
            let started = tokio::time::Instant::now();
            while self.runtime.active.load(Ordering::SeqCst) > 0 {
                if started.elapsed() >= deadline {
                    return Err(
                        "running management commands did not finish within drain_seconds"
                            .to_owned(),
                    );
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        })
    }
}

/// SIGHUP re-reads the files (Unix); elsewhere `spot reload` is the only trigger.
async fn reload_on_hangup(
    runtime: Arc<Runtime>,
    settings: Settings,
    stop: &mut watch::Receiver<bool>,
) {
    #[cfg(unix)]
    {
        let Ok(mut hangup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            return;
        };
        loop {
            tokio::select! {
                _ = hangup.recv() => {
                    if let Err(reason) = runtime.reload(&settings) {
                        tracing::error!(%reason, "management API reload failed; the old configuration stays");
                    }
                }
                _ = stop.changed() => return,
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (runtime, settings, stop);
    }
}

// ---- registration ----------------------------------------------------------------------------

fn runtimes() -> &'static Mutex<HashMap<String, Arc<Runtime>>> {
    static RUNTIMES: OnceLock<Mutex<HashMap<String, Arc<Runtime>>>> = OnceLock::new();
    RUNTIMES.get_or_init(Mutex::default)
}

fn refuse(message: impl std::fmt::Display) -> Error {
    Error(format!("services.mgmt_api: {message}"))
}

/// Registers the service. Off by default; when enabled every start-up failure stops the site.
///
/// # Errors
/// A setting that does not decode or validate, an audit chain or database that is not available, or
/// a catalogue the contributed operations do not form.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("mgmt_api")?;
    let handlers = Arc::new(MgmtHandlers::default());
    ctx.provide::<MgmtHandlers>(Arc::clone(&handlers))?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate().map_err(refuse)?;
    let site = ctx.label().to_owned();
    // The management door's offline decision: per site, survives a reload, filled once every module
    // has registered (empty: unchanged).
    let offline = door_slot(&site, Door::Management);
    finalise_door(ctx, "offline_door_management", {
        let slot = Arc::clone(&offline);
        move |admission| slot.set(admission)
    });
    if !ctx.assembling() {
        // A reload: the service keeps running; it re-reads its files.
        let existing = runtimes()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&site)
            .cloned();
        if let Some(runtime) = existing {
            runtime.reload(&settings).map_err(refuse)?;
        }
        return Ok(());
    }
    let env = *ctx.env()?;
    let pool = env.backend.pool();
    let clock: Arc<dyn Clock> = Arc::clone(ctx.clock());
    let metrics = Arc::new(MgmtMetrics::default());
    let pg = env.loaded.admin.clone().with_dbname(&env.site.database);
    let chain = match crate::audit::chain::ChainAudit::open(pg, &env.site.site, None).await {
        Ok(Ok((chain, _))) => chain,
        _ => {
            return Err(refuse(
                "the identity audit chain is not available; run `spot migrate --apply`",
            ));
        }
    };
    let audit: Arc<dyn AuditSink> = Arc::new(ChainSink {
        recorder: crate::audit::Recorder::new(&env.site.site, Arc::new(chain), Arc::clone(&clock)),
        metrics: Arc::clone(&metrics),
        counter: AtomicU64::new(0),
    });
    let approvals: Arc<dyn ApprovalCheck> = ctx
        .port::<dyn ApprovalCheck>()
        .unwrap_or_else(|| Arc::new(NoApprovals));
    ctx.provide::<MgmtMetrics>(Arc::clone(&metrics))?;
    let site_id = env.site.site.clone();
    ctx.finally(move |ctx| {
        let entries = handlers.take();
        let active = Arc::new(AtomicUsize::new(0));
        let mut builder = Catalogue::builder();
        let mut registry = HandlerRegistry::new();
        for entry in entries {
            let id = entry.spec.id.as_str().to_owned();
            builder.add(entry.module, entry.spec);
            let counted: Arc<dyn Handler> = Arc::new(Counted {
                inner: entry.handler,
                active: Arc::clone(&active),
                offline: Arc::clone(&offline),
            });
            registry
                .register(&id, counted, entry.cancellable)
                .map_err(|_| refuse(format!("operation {id} has two handlers")))?;
        }
        let catalogue = builder.build().map_err(|errors| {
            refuse(format!(
                "the contributed operations do not form a catalogue ({} problems)",
                errors.len()
            ))
        })?;
        let (nonces, nonce_store): (Arc<dyn NonceStore>, Option<Arc<PgNonceStore>>) =
            if settings.store == "postgres" {
                let store = Arc::new(PgNonceStore::with_skew(pool.clone(), MAX_CLOCK_SKEW_SECS));
                (Arc::clone(&store) as Arc<dyn NonceStore>, Some(store))
            } else {
                (
                    Arc::new(spot_signed_command::MemoryNonceStore::new()) as Arc<dyn NonceStore>,
                    None,
                )
            };
        let command_store = Arc::new(PgCommandStore::new(pool));
        let (config, ca) = load_tls(&settings).map_err(refuse)?;
        let keys = load_admin_keys(
            settings
                .administrator_keys
                .as_deref()
                .unwrap_or(Path::new("")),
        )
        .map_err(refuse)?;
        let runtime = Arc::new(Runtime {
            parts: Parts {
                catalogue: Arc::new(catalogue),
                handlers: registry,
                nonces,
                approvals,
                clock: Arc::clone(&clock),
                audit,
                commands: Arc::clone(&command_store) as Arc<dyn CommandStore>,
                site_id,
            },
            tls: ReloadableTls::new(config),
            router: Arc::new(RwLock::new(Router::new())),
            active,
        });
        let first = runtime.build_router(&settings, keys, ca);
        if let Ok(mut current) = runtime.router.write() {
            *current = first;
        }
        runtimes()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(ctx.label().to_owned(), Arc::clone(&runtime));
        ctx.add(Arc::new(MgmtApiService {
            runtime,
            settings,
            nonce_store,
            command_store: Some(command_store),
            metrics,
            clock,
        }));
        Ok(())
    });
    Ok(())
}

#[cfg(test)]
mod offline_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    struct Lapsed;

    impl spot_http::api::service::OfflineAdmission for Lapsed {
        fn admit(
            &self,
            _: Door,
            _: &str,
            _: DoorTier,
            _: Option<&str>,
        ) -> std::result::Result<(), OfflineRefusal> {
            Err(OfflineRefusal::Denied)
        }

        fn node_admitted(&self, _: Door, _: DoorTier) -> std::result::Result<(), OfflineRefusal> {
            Err(OfflineRefusal::Denied)
        }
    }

    #[test]
    fn a_command_is_refused_while_the_node_holds_no_valid_lease_and_a_site_without_offline_is_unchanged()
     {
        let slot = OfflineSlot::default();
        assert_eq!(slot.admit_node(Door::Management, DoorTier::One), Ok(()));
        slot.set(Some(Arc::new(Lapsed)));
        let refusal = slot
            .admit_node(Door::Management, DoorTier::One)
            .unwrap_err();
        assert_eq!(offline_code(refusal), "permission.denied");
        assert_eq!(offline_code(OfflineRefusal::NotReady), "server.unavailable");
    }
}
