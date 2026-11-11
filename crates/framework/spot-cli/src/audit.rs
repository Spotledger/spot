//! The identity audit trail of a site: failed and successful sign-ins, lockouts, password
//! changes and ended sessions.
//!
//! Three pieces:
//!
//! * [`IdentityAudit`], the seam: one `append` of a bounded, typed [`AuditRecord`]. A record has
//!   no free-text payload and no field that could hold a password, a hash, a token or a raw
//!   address, so nothing secret can reach the sink by construction. [`FileAudit`] is the
//!   production sink (append-only file, mode 0600, size-capped rotation); [`CapturingAudit`] is
//!   the sink the tests use.
//! * [`Recorder`], which stamps each record (site, injected time), retries a failed write once,
//!   and bounds growth. Failures, refusals and lockouts can be caused by anyone on the network
//!   without signing in, so they share a per-minute budget; whatever is dropped over the budget
//!   is counted and written as one `failures_suppressed` summary when the minute ends. Records
//!   that follow a successful, authenticated act are never dropped.
//! * [`AuditingEvents`] (the sign-in service's event sink) and [`AuditedThrottleStore`] (which
//!   sees lockout start and end, a transition the service itself does not announce).
//!
//! # Failure behaviour
//!
//! * A *successful* sign-in whose audit write fails (after one retry) is **refused** and the
//!   session it started is revoked: an unauditable session is not acceptable (fail closed).
//! * A *failure* record whose write fails after one retry is logged at error level (kind only)
//!   and the caller still gets the ordinary refusal, so the audit outage never turns into a
//!   difference an unauthenticated caller can observe.
//! * A lockout is applied before it is audited; if the audit write fails the account stays
//!   locked. A lockout is only lifted after the `lockout_ended` record is written.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use spot_core::Clock;
use spot_login::{IdentityEvent, IdentityEvents, LoginAttempt, LoginError, LoginErrorKind};
use spot_sessions::SessionStore;

use crate::{Error, Result};

/// The longest attempted sign-in name an audit record keeps, in characters.
pub const ATTEMPT_NAME_MAX: usize = 128;
/// The most failure-class records (failed attempts, refusals) written per minute.
pub const FAILURES_PER_MINUTE: u32 = 600;
/// The most lockout records written per minute.
pub const LOCKOUTS_PER_MINUTE: u32 = 60;
/// The size at which the audit file rotates.
pub const FILE_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// The rotated files kept next to the live one.
pub const FILE_KEEP: usize = 3;
/// The longest line the file sink writes; a longer record is truncated to a stub.
const LINE_MAX: usize = 2048;
const WINDOW_SECONDS: i64 = 60;

/// One audit record. Every field is bounded and typed; there is no free-text payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuditRecord {
    /// When it happened, unix seconds (injected clock).
    pub at: i64,
    /// The site.
    pub site: String,
    /// What happened: `sign_in_succeeded`, `sign_in_failed`, `sign_in_attempt`, `sign_in_refused`,
    /// `session_started`, `session_revoked`, `password_changed`, `password_change_failed` (reason `password_change_wrong_current`, `password_change_locked` or the refusal code),
    /// `lockout_started`, `lockout_ended`, `api_key_rejected`, `failures_suppressed` or
    /// `identity_<topic>`.
    pub kind: String,
    /// The user's record id, when the event is about a known user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The sign-in name that was *presented* (redacted, at most [`ATTEMPT_NAME_MAX`]
    /// characters); it need not belong to a user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempted: Option<String>,
    /// The salted digest of the source address, never the address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// A short reason class (`bad_credentials`, `locked`, `expired`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The session id (an opaque id, never the token).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// A count: failures at lockout, sessions ended, records suppressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// When a lockout lifts, unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// The public id of a presented API key (at most [`KEY_ID_MAX`] characters), only when the
    /// credential parsed into one. Never the secret part.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// The class of route a refused credential was presented to (`api`, `metrics`,
    /// `realtime`), never the URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// An audit write that failed. The text never carries a record's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditError(pub String);

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where identity audit records go. An `append` that returns `Ok` means the record is durable
/// as far as the sink can tell.
pub trait IdentityAudit: Send + Sync + 'static {
    /// Appends one record.
    ///
    /// # Errors
    ///
    /// [`AuditError`] when the record could not be stored.
    fn append(&self, record: &AuditRecord) -> std::result::Result<(), AuditError>;
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- the file sink -------------------------------------------------------------------------

/// The production sink: JSON lines appended to one file with mode 0600, synced after each
/// record, rotated at [`FILE_MAX_BYTES`] with [`FILE_KEEP`] older files kept. The disk this
/// can use is therefore bounded at `FILE_MAX_BYTES * (FILE_KEEP + 1)`.
pub struct FileAudit {
    path: PathBuf,
    max_bytes: u64,
    state: Mutex<FileState>,
}

struct FileState {
    file: std::fs::File,
    size: u64,
}

fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    // The audit file is never a link: a planted symlink would redirect the trail elsewhere.
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("the audit log path is a symlink"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        // `mode` only applies when the file is created; tighten one that already existed.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

impl FileAudit {
    /// Opens (creating it) the audit file at `path`.
    ///
    /// # Errors
    ///
    /// When the directory or file cannot be created or opened: the host refuses to serve a
    /// site whose audit trail cannot be written.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, FILE_MAX_BYTES)
    }

    /// Like [`FileAudit::open`] with an explicit rotation size.
    ///
    /// # Errors
    ///
    /// As [`FileAudit::open`].
    pub fn open_with(path: &Path, max_bytes: u64) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = open_append(path)
            .map_err(|e| Error(format!("cannot open the identity audit log: {e}")))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            max_bytes: max_bytes.max(1024),
            state: Mutex::new(FileState { file, size }),
        })
    }

    fn rotated(&self, n: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&self, state: &mut FileState) -> std::io::Result<()> {
        // Oldest first, so no step overwrites a file that is still needed.
        let _ = std::fs::remove_file(self.rotated(FILE_KEEP));
        for n in (1..FILE_KEEP).rev() {
            let from = self.rotated(n);
            if from.exists() {
                std::fs::rename(&from, self.rotated(n + 1))?;
            }
        }
        std::fs::rename(&self.path, self.rotated(1))?;
        state.file = open_append(&self.path)?;
        state.size = 0;
        Ok(())
    }
}

impl IdentityAudit for FileAudit {
    fn append(&self, record: &AuditRecord) -> std::result::Result<(), AuditError> {
        let mut line = serde_json::to_string(record)
            .map_err(|_| AuditError("the audit record could not be encoded".into()))?;
        if line.len() > LINE_MAX {
            // Every field is bounded, so this is unreachable; a stub keeps the file well formed.
            line = format!(
                "{{\"at\":{},\"kind\":\"record_too_long\"}}",
                record.at.clamp(0, i64::MAX)
            );
        }
        line.push('\n');
        let mut state = lock(&self.state);
        if state.size.saturating_add(line.len() as u64) > self.max_bytes {
            self.rotate(&mut state)
                .map_err(|e| AuditError(format!("audit log rotation failed: {}", e.kind())))?;
        }
        state
            .file
            .write_all(line.as_bytes())
            .and_then(|()| state.file.sync_data())
            .map_err(|e| AuditError(format!("audit log write failed: {}", e.kind())))?;
        state.size += line.len() as u64;
        Ok(())
    }
}

// ---- the capturing sink --------------------------------------------------------------------

/// A sink that keeps records in memory and can be told to fail, for tests of the audit
/// behaviour. Never used by the host.
#[derive(Default)]
pub struct CapturingAudit {
    records: Mutex<Vec<AuditRecord>>,
    failing: Mutex<u32>,
    always_failing: Mutex<bool>,
}

impl CapturingAudit {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record written so far.
    pub fn records(&self) -> Vec<AuditRecord> {
        lock(&self.records).clone()
    }

    /// The records of one kind.
    pub fn of(&self, kind: &str) -> Vec<AuditRecord> {
        self.records()
            .into_iter()
            .filter(|r| r.kind == kind)
            .collect()
    }

    /// Makes the next `n` appends fail.
    pub fn fail_next(&self, n: u32) {
        *lock(&self.failing) = n;
    }

    /// Makes every append fail (or stops doing so).
    pub fn fail_all(&self, on: bool) {
        *lock(&self.always_failing) = on;
    }
}

impl IdentityAudit for CapturingAudit {
    fn append(&self, record: &AuditRecord) -> std::result::Result<(), AuditError> {
        if *lock(&self.always_failing) {
            return Err(AuditError("capturing sink: failing".into()));
        }
        {
            let mut failing = lock(&self.failing);
            if *failing > 0 {
                *failing -= 1;
                return Err(AuditError("capturing sink: failing".into()));
            }
        }
        lock(&self.records).push(record.clone());
        Ok(())
    }
}

// ---- the recorder --------------------------------------------------------------------------

#[derive(Default)]
struct Window {
    start: i64,
    failures: u32,
    lockouts: u32,
    suppressed: u64,
}

/// Which budget a record is counted against.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Caused by an unauthenticated attempt: limited.
    Failure,
    /// A lockout started or ended: limited, with its own budget.
    Lockout,
}

/// Stamps, writes, retries and bounds the audit records of one site.
pub struct Recorder {
    site: String,
    sink: Arc<dyn IdentityAudit>,
    clock: Arc<dyn Clock>,
    failures_per_minute: u32,
    lockouts_per_minute: u32,
    window: Mutex<Window>,
}

impl Recorder {
    /// A recorder for `site` over `sink`, reading time from `clock`, with the default budgets.
    #[must_use]
    pub fn new(site: &str, sink: Arc<dyn IdentityAudit>, clock: Arc<dyn Clock>) -> Self {
        Self::with_budgets(site, sink, clock, FAILURES_PER_MINUTE, LOCKOUTS_PER_MINUTE)
    }

    /// Like [`Recorder::new`] with explicit per-minute budgets.
    #[must_use]
    pub fn with_budgets(
        site: &str,
        sink: Arc<dyn IdentityAudit>,
        clock: Arc<dyn Clock>,
        failures_per_minute: u32,
        lockouts_per_minute: u32,
    ) -> Self {
        Self {
            site: site.to_owned(),
            sink,
            clock,
            failures_per_minute,
            lockouts_per_minute,
            window: Mutex::new(Window::default()),
        }
    }

    /// The current unix second on the injected clock.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// A record of `kind`, stamped with the site and the time.
    #[must_use]
    pub fn record(&self, kind: &str) -> AuditRecord {
        AuditRecord {
            at: self.now(),
            site: self.site.clone(),
            kind: kind.to_owned(),
            user: None,
            attempted: None,
            source: None,
            reason: None,
            session: None,
            count: None,
            until: None,
            key_id: None,
            path: None,
        }
    }

    /// Writes with one retry.
    fn write(&self, record: &AuditRecord) -> std::result::Result<(), AuditError> {
        self.sink
            .append(record)
            .or_else(|_| self.sink.append(record))
    }

    /// Decides whether a limited record is admitted now, and returns a summary to write when a
    /// minute that dropped records has just ended.
    fn admit(&self, class: Class) -> (bool, Option<AuditRecord>) {
        let now = self.now();
        let mut window = lock(&self.window);
        let mut summary = None;
        if window.start == 0 || now < window.start || now - window.start >= WINDOW_SECONDS {
            if window.suppressed > 0 {
                let mut record = self.record("failures_suppressed");
                record.at = window.start;
                record.count = Some(window.suppressed);
                summary = Some(record);
            }
            *window = Window {
                start: now,
                ..Window::default()
            };
        }
        let (used, budget) = match class {
            Class::Failure => (&mut window.failures, self.failures_per_minute),
            Class::Lockout => (&mut window.lockouts, self.lockouts_per_minute),
        };
        if *used >= budget {
            window.suppressed += 1;
            return (false, summary);
        }
        *used += 1;
        (true, summary)
    }

    fn limited(&self, class: Class, record: &AuditRecord) {
        let (admitted, summary) = self.admit(class);
        if let Some(summary) = summary {
            self.always(&summary);
        }
        if admitted {
            self.always(record);
        }
    }

    /// Records something an unauthenticated caller can cause: counted against the per-minute
    /// budget, retried once, logged (kind only) if it still fails. Never errors.
    pub fn failure(&self, record: &AuditRecord) {
        self.limited(Class::Failure, record);
    }

    /// Records a lockout start or end: own budget, same failure behaviour as [`failure`].
    ///
    /// [`failure`]: Recorder::failure
    pub fn lockout(&self, record: &AuditRecord) {
        self.limited(Class::Lockout, record);
    }

    /// Records something that follows an authenticated act: never dropped; retried once and
    /// logged (kind only) if it still fails. Never errors.
    pub fn always(&self, record: &AuditRecord) {
        if self.write(record).is_err() {
            tracing::error!(
                site = %record.site,
                kind = %record.kind,
                "identity audit write failed after a retry; the event is not in the audit trail"
            );
        }
    }

    /// Records something the act depends on: retried once, and an error when it still fails.
    ///
    /// # Errors
    ///
    /// [`AuditError`] when both attempts failed; the caller must refuse the act.
    pub fn required(&self, record: &AuditRecord) -> std::result::Result<(), AuditError> {
        self.write(record).inspect_err(|_| {
            tracing::error!(
                site = %record.site,
                kind = %record.kind,
                "identity audit write failed after a retry; the act is refused"
            );
        })
    }
}

// ---- redaction -----------------------------------------------------------------------------

/// The sign-in name as an audit record may keep it: control and non-name characters are
/// replaced, a string that looks like a credential (long, no separator) is dropped entirely,
/// and the result is cut at [`ATTEMPT_NAME_MAX`] characters. People paste passwords and API keys
/// into the name field; the audit must not keep them.
#[must_use]
pub fn redact_name(presented: &str) -> String {
    let trimmed = presented.trim();
    let alnum_run = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if trimmed.chars().count() > 40 && alnum_run {
        return "[redacted]".into();
    }
    // Token-shaped: no space, not an address, longer than a person's login name. Catches hex
    // keys, JWTs and base64 (which carry `.`, `+`, `/`, `=` and so escape the run test above).
    let token_shaped = trimmed.chars().count() > 24
        && !trimmed.contains('@')
        && !trimmed.chars().any(char::is_whitespace);
    if token_shaped || trimmed.to_ascii_lowercase().starts_with("spot_") {
        return "[redacted]".into();
    }
    trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | '+') {
                c
            } else {
                '?'
            }
        })
        .take(ATTEMPT_NAME_MAX)
        .collect()
}

fn reason_class(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_lowercase() || matches!(c, '_' | '.'))
        .take(48)
        .collect()
}

// ---- the identity events sink --------------------------------------------------------------

/// The sign-in service's event sink: one audit record per identity event and per attempt.
pub struct AuditingEvents {
    recorder: Arc<Recorder>,
    sessions: Arc<dyn SessionStore>,
}

impl AuditingEvents {
    /// Events audited through `recorder`; `sessions` is where a session is revoked when the
    /// audit of its sign-in cannot be written.
    #[must_use]
    pub fn new(recorder: Arc<Recorder>, sessions: Arc<dyn SessionStore>) -> Self {
        Self { recorder, sessions }
    }

    async fn revoke(&self, session: &str) {
        if self
            .sessions
            .revoke(session, "audit_unavailable".to_owned())
            .await
            .is_err()
        {
            tracing::error!(
                "a session whose sign-in could not be audited could not be revoked either"
            );
        }
    }
}

fn session_of(event: &IdentityEvent) -> Option<String> {
    event
        .data
        .get("session")
        .and_then(serde_json::Value::as_str)
        .filter(|s| {
            s.len() <= 64
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        })
        .map(str::to_owned)
}

fn unavailable() -> LoginError {
    LoginError::new(
        LoginErrorKind::StoreFailed,
        "the audit trail is unavailable",
    )
}

impl IdentityEvents for AuditingEvents {
    fn emit<'a>(
        &'a self,
        event: IdentityEvent,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), LoginError>> + Send + 'a>> {
        Box::pin(async move {
            let user = event.subject.map(|id| id.to_string());
            let reason = event
                .data
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .map(reason_class);
            let session = session_of(&event);
            match event.topic {
                "identity_session_started" | "identity_sign_in_succeeded" => {
                    let kind = if event.topic == "identity_session_started" {
                        "session_started"
                    } else {
                        "sign_in_succeeded"
                    };
                    let mut record = self.recorder.record(kind);
                    record.user = user;
                    record.session = session.clone();
                    if self.recorder.required(&record).is_err() {
                        if let Some(session) = &session {
                            self.revoke(session).await;
                        }
                        return Err(unavailable());
                    }
                }
                "identity_sign_in_failed" => {
                    let mut record = self.recorder.record("sign_in_failed");
                    record.user = user;
                    record.reason = reason;
                    self.recorder.failure(&record);
                }
                "identity_session_ended" => {
                    let mut record = self.recorder.record("session_revoked");
                    record.user = user;
                    record.session = session;
                    record.reason = reason;
                    self.recorder.always(&record);
                }
                "identity_password_changed" => {
                    let mut record = self.recorder.record("password_changed");
                    record.user = user;
                    record.count = event
                        .data
                        .get("sessions_ended")
                        .and_then(serde_json::Value::as_u64);
                    self.recorder.always(&record);
                }
                "identity_account_unlocked" => {
                    let mut record = self.recorder.record("lockout_ended");
                    record.user = user;
                    record.reason = Some("admin_unlock".into());
                    self.recorder.lockout(&record);
                }
                other => {
                    let topic = other.strip_prefix("identity_").unwrap_or(other);
                    let mut record = self
                        .recorder
                        .record(&format!("identity_{}", reason_class(topic)));
                    record.user = user;
                    self.recorder.always(&record);
                }
            }
            Ok(())
        })
    }

    fn record_login_attempt<'a>(
        &'a self,
        attempt: LoginAttempt,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), LoginError>> + Send + 'a>> {
        Box::pin(async move {
            let code = attempt.result_code.as_str();
            let result = code.strip_prefix("login.").unwrap_or(code);
            let mut record = self.recorder.record(match result {
                "account_locked" | "throttled" | "throttle_burst" => "sign_in_refused",
                _ => "sign_in_attempt",
            });
            record.at = attempt.at.unix_timestamp();
            record.attempted = Some(redact_name(&attempt.account));
            record.source = Some(
                attempt
                    .source_digest
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(64)
                    .collect(),
            );
            record.reason = Some(reason_class(result));
            if result == spot_login::RESULT_OK {
                // The success itself is audited, fail closed, by the session events.
                self.recorder.always(&record);
            } else {
                self.recorder.failure(&record);
            }
            Ok(())
        })
    }
}

pub mod chain;
mod key_rejection;
pub use key_rejection::{KEY_ID_MAX, PathClass, RejectionReason, classify, rejected};
mod throttle;
pub use throttle::{AuditedThrottleStore, BoundedThrottleStore, THROTTLE_KEY_CAP};

#[cfg(test)]
mod tests;
