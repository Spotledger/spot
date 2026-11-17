//! The vocabulary of the break-glass host module: its codes and errors, the ports it reads, its
//! settings and the registry's plain types (WP-ADM-11.T2).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_audit_chain::ChainEntry;
use spot_break_glass::{BreakGlassError, BreakGlassEvent, Frame, SessionSummary};

use crate::{Error, Result};

/// The duty that reads recordings and revokes sessions.
pub const ADMIN_DUTY: &str = "security_administrator";
/// The duty that may also list sessions and read a summary.
pub const AUDITOR_DUTY: &str = "security_auditor";

/// The `recording_read` event kind: written when a customer administrator reads a recording.
/// The chain prefixes it with `break_glass.`.
pub const READ_KIND: &str = "recording_read";

/// The stable codes this module answers with (see `catalogue/break_glass_host.toml`); the
/// crate's own refusals keep their `break_glass.*` codes.
pub mod code {
    /// `[services.break_glass]` has an unknown key or an invalid value.
    pub const SETTINGS_INVALID: &str = "break_glass_host.settings_invalid";
    /// A part the operation needs is not wired in this host.
    pub const NOT_WIRED: &str = "break_glass_host.not_wired";
    /// The caller does not hold the duty (or the duty could not be checked).
    pub const DENIED: &str = "break_glass_host.permission_denied";
    /// No such session (a session of another tenant answers the same).
    pub const NOT_FOUND: &str = "break_glass_host.not_found";
    /// The object store refused or failed.
    pub const STORE_FAILED: &str = "break_glass_host.store_failed";
    /// The recording could not be sealed or opened.
    pub const SEAL_FAILED: &str = "break_glass_host.seal_failed";
    /// An input is out of bounds.
    pub const INVALID: &str = "break_glass_host.invalid";
}

/// Why an operation was refused. A fixed sentence per case: no id, key, path or content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostError {
    /// The duty is not held, or could not be checked.
    Denied,
    /// No such session or recording in this tenant.
    NotFound,
    /// This part is not wired.
    NotWired(&'static str),
    /// The object store failed.
    Store,
    /// Sealing or opening failed.
    Seal,
    /// An input is out of bounds (the reason is a fixed phrase).
    Invalid(&'static str),
    /// A refusal of the break-glass crate.
    Crate(BreakGlassError),
}

impl HostError {
    /// The stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Denied => code::DENIED,
            Self::NotFound => code::NOT_FOUND,
            Self::NotWired(_) => code::NOT_WIRED,
            Self::Store => code::STORE_FAILED,
            Self::Seal => code::SEAL_FAILED,
            Self::Invalid(_) => code::INVALID,
            Self::Crate(e) => e.code(),
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => write!(f, "{}: the caller may not do that", self.code()),
            Self::NotFound => write!(f, "{}: no such break-glass session", self.code()),
            Self::NotWired(part) => write!(f, "{}: `{part}` is not wired", self.code()),
            Self::Store => write!(f, "{}: the object store failed", self.code()),
            Self::Seal => write!(
                f,
                "{}: the recording could not be sealed or opened",
                self.code()
            ),
            Self::Invalid(why) => write!(f, "{}: {why}", self.code()),
            Self::Crate(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<BreakGlassError> for HostError {
    /// An unknown session is the same answer as a missing one.
    fn from(e: BreakGlassError) -> Self {
        match e {
            BreakGlassError::UnknownSession => Self::NotFound,
            other => Self::Crate(other),
        }
    }
}

/// A boxed future, as the ports return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------------------------
// The parts the host provides (ports)
// ---------------------------------------------------------------------------------------------

/// The tenant's audit chain, as this module uses it. The implementation appends durably before
/// it answers `Ok` and writes `break_glass.<kind>` records (`ChainLog` of the crate does).
pub trait EventLog: Send + Sync {
    /// Appends one event.
    fn append<'a>(
        &'a self,
        event: BreakGlassEvent,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>>;
    /// The tenant's chain entries (the whole chain; the crate's view skips what is not its own).
    fn entries<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<ChainEntry>, HostError>>;
}

/// Append-only objects in the tenant's file store. There is no overwrite and no delete here.
pub trait ObjectStore: Send + Sync {
    /// Writes `bytes` under `key`; `Err(HostError::Crate(AppendOnly))` when it exists.
    fn put_new<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>>;
    /// Reads `key`; `None` when absent.
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<Vec<u8>>, HostError>>;
    /// The keys that start with `prefix`.
    fn list<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Vec<String>, HostError>>;
}

/// Removal of an expired object. Held only by the retention sweep, never by a port.
pub trait Retire: Send + Sync {
    /// Removes `key` (absent is fine).
    fn remove<'a>(&'a self, key: &'a str) -> BoxFuture<'a, std::result::Result<(), HostError>>;
}

/// Sealing of recordings by the secrets service. `context` is bound to the ciphertext
/// (tenant and session), so a sealed recording cannot be moved to another session.
pub trait Sealer: Send + Sync {
    /// Seals `plain`.
    fn seal(&self, context: &str, plain: &[u8]) -> std::result::Result<Vec<u8>, HostError>;
    /// Opens `sealed`; a wrong context or a changed byte is `Err`.
    fn open(&self, context: &str, sealed: &[u8]) -> std::result::Result<Vec<u8>, HostError>;
}

/// The duty check of the site's permission engine.
pub trait Duties: Send + Sync {
    /// Whether `user` of `tenant` holds `duty`. An `Err` is a refusal.
    fn holds<'a>(
        &'a self,
        tenant: &'a str,
        user: &'a str,
        duty: &'a str,
    ) -> BoxFuture<'a, std::result::Result<bool, HostError>>;
}

/// The in-process port to the agent bridge that carries a kill to the host agent.
pub trait AgentBridge: Send + Sync {
    /// Delivers the kill of `session`. `Err` means the bridge could not be reached.
    fn kill<'a>(
        &'a self,
        tenant: &'a str,
        session: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>>;
}

/// Retention of recordings from the tenant's data policy.
pub trait Retention: Send + Sync {
    /// Days to keep recordings of `tenant`; `None` when the policy names none.
    fn recording_days(&self, tenant: &str) -> Option<u32>;
}

/// The parts the host supplies. A missing one degrades the service and is named by the check.
#[derive(Clone, Default)]
pub struct Parts {
    /// The audit chain.
    pub log: Option<Arc<dyn EventLog>>,
    /// The append-only object store.
    pub store: Option<Arc<dyn ObjectStore>>,
    /// Object removal for retention (optional: without it retention is not enforced).
    pub retire: Option<Arc<dyn Retire>>,
    /// The sealer.
    pub sealer: Option<Arc<dyn Sealer>>,
    /// The duty check.
    pub duties: Option<Arc<dyn Duties>>,
    /// The agent bridge (optional: without it a kill is always "not delivered").
    pub bridge: Option<Arc<dyn AgentBridge>>,
    /// The data policy's retention (optional: the setting is the fallback).
    pub retention: Option<Arc<dyn Retention>>,
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// `[services.break_glass]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site hosts the customer side of break-glass.
    pub enabled: bool,
    /// Seconds between sweeps (1 to 3600).
    pub sweep_secs: u64,
    /// Fallback retention of recordings, in days (1 to 3650).
    pub retention_days: u32,
    /// The largest recording accepted, in bytes (1 to 1 GiB).
    pub max_recording_bytes: u64,
    /// The largest page a list returns (1 to 100).
    pub page_max: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            sweep_secs: 15,
            retention_days: 365,
            max_recording_bytes: 64 * 1024 * 1024,
            page_max: 100,
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
                "{}: services.break_glass.{key} is out of bounds",
                code::SETTINGS_INVALID
            ))
        };
        if !(1..=3600).contains(&self.sweep_secs) {
            return Err(bad("sweep_secs"));
        }
        if !(1..=3650).contains(&self.retention_days) {
            return Err(bad("retention_days"));
        }
        if self.max_recording_bytes == 0 || self.max_recording_bytes > 1 << 30 {
            return Err(bad("max_recording_bytes"));
        }
        if !(1..=100).contains(&self.page_max) {
            return Err(bad("page_max"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The registry and its port
// ---------------------------------------------------------------------------------------------

/// The window of one session, registered when it is activated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// The tenant (must be this site).
    pub tenant: String,
    /// The session id.
    pub session: String,
    /// Who asked (shown to the customer).
    pub requester: String,
    /// First instant the session may act (unix seconds).
    pub starts_at: i64,
    /// First instant it may not (unix seconds).
    pub expires_at: i64,
}

/// The customer-side caller of a list, read, recording or revoke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The tenant the caller belongs to.
    pub tenant: String,
    /// The user's id.
    pub user: String,
}

/// A page request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The id of the last session of the previous page.
    pub after: Option<String>,
    /// The page size.
    pub limit: usize,
}

/// One page of sessions, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// The sessions.
    pub rows: Vec<SessionSummary>,
    /// The cursor of the next page, if there is one.
    pub next: Option<String>,
}

/// What a revoke did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevokeOutcome {
    /// The kill reached the agent bridge. When `false` the session is revoked all the same and
    /// the sweep keeps retrying.
    pub delivered: bool,
    /// The `revoked` event is on the audit chain. When `false` it is queued and retried.
    pub audited: bool,
}

/// What a recording read returns: the verified frames, never anything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingView {
    /// The session's summary.
    pub summary: SessionSummary,
    /// The head digest, as hex (equal to the one on the chain).
    pub digest: String,
    /// Whether the recording carries the truncation marker.
    pub truncated: bool,
    /// The frames.
    pub frames: Vec<Frame>,
}

/// What a sweep did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Queued events written to the chain.
    pub flushed: usize,
    /// Sessions ended because their window closed.
    pub expired: usize,
    /// Kills delivered on a retry (revoked or expired sessions).
    pub kills: usize,
    /// Recordings removed after their retention.
    pub retired: usize,
}

/// The port the support-access side and the routes use.
pub trait BreakGlassPort: Send + Sync {
    /// Registers the window of an activated session (once; the same window again is a no-op).
    fn register<'a>(&'a self, window: Window) -> BoxFuture<'a, std::result::Result<(), HostError>>;
    /// Whether `session` may act at `now`. Never cached: a revoked, expired, unknown or
    /// not-yet-started session, or a host that is not wired, is a refusal.
    fn authorise<'a>(
        &'a self,
        session: &'a str,
        now: i64,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>>;
    /// Writes an event the support side reports (ended, command, recording, ...). A terminal
    /// kind closes the session here before the event is written.
    fn record_event<'a>(
        &'a self,
        event: BreakGlassEvent,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>>;
    /// Stores a finished recording and writes its digest to the chain.
    fn archive_recording<'a>(
        &'a self,
        session: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, std::result::Result<String, HostError>>;
    /// The customer's list of sessions, active and past.
    fn list<'a>(
        &'a self,
        caller: &'a Caller,
        page: Page,
    ) -> BoxFuture<'a, std::result::Result<Listing, HostError>>;
    /// One session's summary.
    fn summary<'a>(
        &'a self,
        caller: &'a Caller,
        session: &'a str,
    ) -> BoxFuture<'a, std::result::Result<SessionSummary, HostError>>;
    /// A session's recording (audited).
    fn recording<'a>(
        &'a self,
        caller: &'a Caller,
        session: &'a str,
    ) -> BoxFuture<'a, std::result::Result<RecordingView, HostError>>;
    /// Revokes an active session.
    fn revoke<'a>(
        &'a self,
        caller: &'a Caller,
        session: &'a str,
    ) -> BoxFuture<'a, std::result::Result<RevokeOutcome, HostError>>;
}
