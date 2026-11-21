//! Break-glass on the host agent (WP-ADM-11.T3): `breakglass.open` and `breakglass.close`.
//!
//! The control plane decides who may ask and approves with two administrators; **the agent
//! enforces the end**. `breakglass.open` creates a temporary account scoped to exactly the grant,
//! starts the hash-chained recorder and arms an expiry on the agent's own monotonic clock, so a
//! lost connection still closes the session at its expiry. `breakglass.close` (and the expiry)
//! removes the account, seals the recording and writes its digest next to it.
//!
//! # Rules
//!
//! - Expiry is the agent's: [`BreakGlassAgent::tick`] closes every session past its monotonic
//!   deadline; a timer thread calls it every second and tests call it after moving a manual clock.
//! - A session id is used once: a closed or expired session can never be reopened (the used ids
//!   are an append-only file under the state directory, read again at start).
//! - The recording is a `spot-break-glass` frame log: input typed with echo off is replaced by a
//!   marker before it is written, so a typed password never reaches the file.
//! - Every step, refusals and expiry included, is a line of `audit_log` in the session
//!   directory, hash-chained (`SHA-256(prev, line)`); a line holds no argument value and no
//!   terminal bytes.
//! - Fail closed: no account provider means the open is refused (the production provider is the
//!   privilege helper's templates, which a later task adds, see the task report).
//!
//! `HandlerOutcome::Succeeded` carries no payload, so the digest is written to
//! `<session>.digest` and to the audit line; the channel's uploader reads it from there.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use spot_agent_core::{ExecutionPlan, HandlerOutcome, HandlerRegistry, OperationHandler};
use spot_break_glass::{FileRecorder, FrameKind, Recorder, hex};
use spot_core::Clock;
use spot_mgmt_ops::{
    CatalogueBuilder, OperationSpec, ParamSpec, ParamType, ParamValue, Risk, TargetKind,
};

use super::HandlerCtx;

/// The operation namespace.
pub const NAMESPACE: &str = "breakglass";
/// Opens a session.
pub const OPEN: &str = "breakglass.open";
/// Closes a session.
pub const CLOSE: &str = "breakglass.close";
const MODULE: &str = "spot-agent-breakglass";
/// The longest grant, in seconds (four hours).
pub const MAX_SECONDS: i64 = 4 * 3600;
/// The recording size cap.
const MAX_RECORDING_BYTES: u64 = 64 * 1024 * 1024;

/// Why a break-glass step was refused. Fixed sentences: no argument value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgError {
    /// The session id has been used (open or closed): it cannot be opened again.
    Reused,
    /// The session is not open.
    NotOpen,
    /// The duration is out of range.
    Duration,
    /// An argument is not valid.
    Argument,
    /// The temporary account could not be created or removed.
    Account,
    /// The recording could not be written.
    Recording,
    /// The audit line could not be written: nothing proceeds without it.
    Audit,
}

impl BgError {
    /// The short, secret-free sentence a failed outcome carries.
    pub fn sentence(self) -> &'static str {
        match self {
            Self::Reused => "the session id was already used and cannot be opened again",
            Self::NotOpen => "the session is not open",
            Self::Duration => "the duration is out of range",
            Self::Argument => "an argument is not valid",
            Self::Account => "the temporary account could not be created or removed",
            Self::Recording => "the recording could not be written",
            Self::Audit => "the audit line could not be written",
        }
    }
}

/// Creates and removes the temporary account or key of a session. The production provider is
/// the privilege helper; until its templates exist the agent holds [`RefusingAccounts`].
pub trait SessionAccounts: Send + Sync {
    /// Creates the account, valid for `valid_secs` at most, with exactly `scope`; returns its
    /// name.
    ///
    /// # Errors
    ///
    /// Any failure refuses the open.
    fn create(&self, session: &str, scope: &[String], valid_secs: i64) -> Result<String, BgError>;
    /// Removes the account (absent is fine).
    ///
    /// # Errors
    ///
    /// A failure is audited and retried by the next tick.
    fn remove(&self, session: &str, account: &str) -> Result<(), BgError>;
}

/// The provider of an agent that has none: every open is refused.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefusingAccounts;

impl SessionAccounts for RefusingAccounts {
    fn create(&self, _: &str, _: &[String], _: i64) -> Result<String, BgError> {
        Err(BgError::Account)
    }
    fn remove(&self, _: &str, _: &str) -> Result<(), BgError> {
        Ok(())
    }
}

struct Live {
    tenant: String,
    account: String,
    deadline: Duration,
    recorder: FileRecorder,
    removal_pending: bool,
}

#[derive(Default)]
struct State {
    live: BTreeMap<String, Live>,
    used: BTreeSet<String>,
    audit_seq: u64,
    audit_prev: [u8; 32],
}

/// The agent's break-glass sessions.
pub struct BreakGlassAgent {
    clock: Arc<dyn Clock>,
    accounts: Arc<dyn SessionAccounts>,
    dir: PathBuf,
    state: Mutex<State>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

impl BreakGlassAgent {
    /// An agent keeping its files in `dir`. The used session ids and the audit head are read
    /// back, so a restart cannot reopen a closed session.
    pub fn new(clock: Arc<dyn Clock>, accounts: Arc<dyn SessionAccounts>, dir: &Path) -> Self {
        let mut state = State::default();
        if let Ok(text) = fs::read_to_string(dir.join("used_sessions")) {
            state.used = text.lines().map(str::to_owned).collect();
        }
        if let Ok(text) = fs::read_to_string(dir.join("audit_log")) {
            for line in text.lines() {
                state.audit_prev =
                    Sha256::digest([state.audit_prev.as_slice(), line.as_bytes()].concat()).into();
                state.audit_seq += 1;
            }
        }
        Self {
            clock,
            accounts,
            dir: dir.to_path_buf(),
            state: Mutex::new(state),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// Appends one hash-chained audit line: `seq prev_hash event session detail`. No value of an
    /// argument and no terminal byte is ever passed here.
    fn audit(&self, event: &str, session: &str, detail: &str) -> Result<(), BgError> {
        fs::create_dir_all(&self.dir).map_err(|_| BgError::Audit)?;
        let mut st = self.state();
        let line = format!(
            "{} {} {} {} {} {}",
            st.audit_seq + 1,
            hex(&st.audit_prev),
            self.now(),
            event,
            session,
            detail
        );
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("audit_log"))
            .map_err(|_| BgError::Audit)?;
        writeln!(file, "{line}").map_err(|_| BgError::Audit)?;
        file.sync_all().map_err(|_| BgError::Audit)?;
        st.audit_prev = Sha256::digest([st.audit_prev.as_slice(), line.as_bytes()].concat()).into();
        st.audit_seq += 1;
        Ok(())
    }

    fn mark_used(&self, session: &str) -> Result<(), BgError> {
        fs::create_dir_all(&self.dir).map_err(|_| BgError::Audit)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("used_sessions"))
            .map_err(|_| BgError::Audit)?;
        writeln!(file, "{session}").map_err(|_| BgError::Audit)?;
        file.sync_all().map_err(|_| BgError::Audit)?;
        self.state().used.insert(session.to_owned());
        Ok(())
    }

    /// Opens a session. Every refusal is audited too.
    ///
    /// # Errors
    ///
    /// [`BgError`]; nothing is created on a refusal.
    pub fn open(
        &self,
        session: &str,
        tenant: &str,
        scope: &[String],
        seconds: i64,
    ) -> Result<String, BgError> {
        let refuse = |why: BgError| {
            let _ = self.audit("open_refused", session, why.sentence());
            Err(why)
        };
        if !valid_id(session)
            || !valid_id(tenant)
            || scope.is_empty()
            || scope.len() > 16
            || scope.iter().any(|s| !valid_id(s))
        {
            return refuse(BgError::Argument);
        }
        if !(1..=MAX_SECONDS).contains(&seconds) {
            return refuse(BgError::Duration);
        }
        if self.state().used.contains(session) {
            return refuse(BgError::Reused);
        }
        // Mark first: whatever happens next, this id can never open again.
        self.mark_used(session)?;
        let account = match self.accounts.create(session, scope, seconds) {
            Ok(account) => account,
            Err(why) => return refuse(why),
        };
        let recorder = match FileRecorder::create(
            &self.dir.join("recordings"),
            session,
            tenant,
            self.now(),
            MAX_RECORDING_BYTES,
        ) {
            Ok(recorder) => recorder,
            Err(_) => {
                let _ = self.accounts.remove(session, &account);
                return refuse(BgError::Recording);
            }
        };
        let deadline =
            self.clock.monotonic() + Duration::from_secs(u64::try_from(seconds).unwrap_or(0));
        if let Err(why) = self.audit(
            "opened",
            session,
            "scoped account created and recorder started",
        ) {
            let _ = self.accounts.remove(session, &account);
            return Err(why);
        }
        self.state().live.insert(
            session.to_owned(),
            Live {
                tenant: tenant.to_owned(),
                account: account.clone(),
                deadline,
                recorder,
                removal_pending: false,
            },
        );
        Ok(account)
    }

    /// Records bytes the operator typed or the terminal printed. Echo off masks the input.
    ///
    /// # Errors
    ///
    /// [`BgError::NotOpen`] for a closed or unknown session, [`BgError::Recording`] when the frame
    /// cannot be written (the caller must end the session: no unrecorded action).
    pub fn record(
        &self,
        session: &str,
        kind: FrameKind,
        data: &[u8],
        echo: bool,
    ) -> Result<(), BgError> {
        let at = self.now();
        let mut st = self.state();
        let live = st.live.get_mut(session).ok_or(BgError::NotOpen)?;
        live.recorder
            .append(kind, at, data, echo)
            .map(|_| ())
            .map_err(|_| BgError::Recording)
    }

    /// Closes a session now: removes the account, seals the recording and returns its digest.
    ///
    /// # Errors
    ///
    /// [`BgError::NotOpen`], or [`BgError::Recording`] when the seal fails (the account is removed
    /// first either way).
    pub fn close(&self, session: &str, reason: &str) -> Result<String, BgError> {
        let Some(mut live) = self.state().live.remove(session) else {
            let _ = self.audit("close_refused", session, BgError::NotOpen.sentence());
            return Err(BgError::NotOpen);
        };
        // The account goes first: the access ends before anything else can fail.
        if self.accounts.remove(session, &live.account).is_err() {
            live.removal_pending = true;
            let _ = self.audit(
                "account_removal_failed",
                session,
                "will retry on the next tick",
            );
            self.state().live.insert(session.to_owned(), live);
            return Err(BgError::Account);
        }
        let digest = live.recorder.finish().map_err(|_| BgError::Recording)?;
        let digest_hex = hex(&digest);
        let _ = fs::write(self.dir.join(format!("{session}.digest")), &digest_hex);
        self.audit(
            reason,
            session,
            &format!(
                "closed, recording sealed {digest_hex} tenant {}",
                live.tenant
            ),
        )?;
        Ok(digest_hex)
    }

    /// The agent's own timer: closes every session past its monotonic deadline and retries a
    /// removal that failed. Returns the sessions closed.
    pub fn tick(&self) -> Vec<String> {
        let now = self.clock.monotonic();
        let due: Vec<String> = self
            .state()
            .live
            .iter()
            .filter(|(_, l)| now >= l.deadline || l.removal_pending)
            .map(|(id, _)| id.clone())
            .collect();
        let mut closed = Vec::new();
        for session in due {
            if self.close(&session, "expired").is_ok() {
                closed.push(session);
            }
        }
        closed
    }

    /// Whether `session` is open.
    pub fn is_open(&self, session: &str) -> bool {
        self.state().live.contains_key(session)
    }

    /// Starts the timer thread: [`BreakGlassAgent::tick`] every second, for the life of the
    /// process. A lost connection does not stop it.
    pub fn spawn_timer(self: &Arc<Self>) {
        let agent = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("breakglass-expiry".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    agent.tick();
                }
            });
    }
}

fn text(plan: &ExecutionPlan, name: &str) -> Option<String> {
    plan.args()
        .params
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| match &p.value {
            ParamValue::Text(t) => Some(t.clone()),
            _ => None,
        })
}

fn int(plan: &ExecutionPlan, name: &str) -> Option<i64> {
    plan.args()
        .params
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| match &p.value {
            ParamValue::Int(i) => Some(*i),
            _ => None,
        })
}

fn scope(plan: &ExecutionPlan) -> Vec<String> {
    plan.args()
        .params
        .iter()
        .find(|p| p.name == "scope")
        .map(|p| match &p.value {
            ParamValue::List(items) => items
                .iter()
                .filter_map(|v| match v {
                    ParamValue::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

struct Open(Arc<BreakGlassAgent>);
struct Close(Arc<BreakGlassAgent>);

fn failed(error: BgError) -> HandlerOutcome {
    HandlerOutcome::Failed {
        detail: error.sentence().to_owned(),
    }
}

impl OperationHandler for Open {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        let (Some(session), Some(tenant), Some(seconds)) = (
            text(plan, "session"),
            text(plan, "tenant"),
            int(plan, "duration_seconds"),
        ) else {
            return failed(BgError::Argument);
        };
        match self.0.open(&session, &tenant, &scope(plan), seconds) {
            Ok(_) => HandlerOutcome::Succeeded,
            Err(error) => failed(error),
        }
    }
}

impl OperationHandler for Close {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        let Some(session) = text(plan, "session") else {
            return failed(BgError::Argument);
        };
        match self.0.close(&session, "closed") {
            Ok(_) => HandlerOutcome::Succeeded,
            Err(error) => failed(error),
        }
    }
}

/// Registers the two handlers over `agent`.
pub fn register_with(reg: &mut HandlerRegistry, agent: &Arc<BreakGlassAgent>) {
    if let Some(op) = spot_mgmt_ops::OpId::new(OPEN) {
        reg.register(op, Arc::new(Open(Arc::clone(agent))));
    }
    if let Some(op) = spot_mgmt_ops::OpId::new(CLOSE) {
        reg.register(op, Arc::new(Close(Arc::clone(agent))));
    }
}

/// Registers the handlers (production: no account provider yet, so every open is refused and the
/// site is unaffected; the expiry timer runs for sessions a provider will open).
pub fn register(reg: &mut HandlerRegistry, ctx: &HandlerCtx) {
    let agent = Arc::new(BreakGlassAgent::new(
        Arc::clone(&ctx.clock),
        Arc::new(RefusingAccounts),
        &ctx.state_dir.join("breakglass"),
    ));
    agent.spawn_timer();
    register_with(reg, &agent);
}

fn param(name: &str, ty: ParamType, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty,
        required: true,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

fn id_text() -> ParamType {
    ParamType::Text {
        max_len: 64,
        pattern: Some("[A-Za-z0-9_-]{1,64}".to_owned()),
    }
}

/// The operations of this set.
pub fn register_catalogue(builder: &mut CatalogueBuilder) {
    let specs = [
        (
            OPEN,
            "Open a break-glass session",
            "Creates the scoped temporary account, starts the recording and arms the expiry on this server.",
            vec![
                param(
                    "session",
                    id_text(),
                    "The session id the control plane granted.",
                ),
                param("tenant", id_text(), "The tenant the session is about."),
                param(
                    "scope",
                    ParamType::List(Box::new(id_text())),
                    "What the session may touch.",
                ),
                param(
                    "duration_seconds",
                    ParamType::Int {
                        min: 1,
                        max: MAX_SECONDS,
                    },
                    "How long the session lasts, at most four hours.",
                ),
            ],
        ),
        (
            CLOSE,
            "Close a break-glass session",
            "Removes the temporary account and seals the recording.",
            vec![param("session", id_text(), "The session to close.")],
        ),
    ];
    for (id, title, summary, params) in specs {
        if let Some(id) = spot_mgmt_ops::OpId::new(id) {
            builder.add(
                MODULE,
                OperationSpec {
                    id,
                    title: title.to_owned(),
                    summary: summary.to_owned(),
                    params,
                    risk: Risk::Critical,
                    target: TargetKind::Server,
                },
            );
        }
    }
}
