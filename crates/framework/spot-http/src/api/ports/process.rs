//! The process and run seam of `process_list`, `run_start`, `run_list`, `run_get`, `run_events`
//! and `run_cancel` (native API 0.12.0, WP-BPM-02.T5): the processes the caller may start, the
//! runs the caller may see, and the starting and cancelling of a run.
//!
//! `spot-http` does not depend on the process engine. It defines [`ProcessPort`], the six
//! operations in the shapes the router needs, and the host writes the adapter over the process
//! host (WP-BPM-02.T6). **Everything that decides is the port's:** who the caller is (from
//! `ctx.credential` on `ctx.site`), which processes the caller's roles let them start, which runs
//! they may see, who may cancel, and the audit of every write, a refusal included. The routes
//! pass **no user, no role and no tenant**: there is no way to ask for another caller's runs, or
//! another site's, through this trait.
//!
//! # One answer for "not there" and "not yours"
//!
//! A process the caller may not start is [`ProcessPortError::NotFound`], exactly as one that does
//! not exist. A run the caller may not see is [`ProcessPortError::RunNotFound`], exactly as one
//! that does not exist. The routes add nothing that tells the two apart.
//!
//! # What is never carried
//!
//! A [`Run`] has no variables, no node outputs and no error text; a failed run has only a stable
//! `failure_code`. A [`RunEvent`] has no payload. A [`ProcessPortError::InputInvalid`] carries
//! field paths and stable codes, never a submitted value.
//!
//! [`MemoryProcessPort`] is the in-memory fake the route tests and the conformance walk use. It
//! implements the visibility and cancel rules of the contract well enough to test the routes; it
//! reads no clock (its timestamps are a deterministic counter) and keeps every site apart.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use serde_json::{Map, Value as Json, json};

use super::PortKey;
use crate::api::service::{BoxFuture, FieldIssue, RequestCtx};

/// The port key of [`ProcessPort`].
pub const KEY: PortKey = PortKey::of::<dyn ProcessPort>();

/// The default and largest page of `process_list` and `run_list`.
pub const DEFAULT_PAGE: u32 = 50;
/// The largest page of `process_list` and `run_list`.
pub const MAX_PAGE: u32 = 100;
/// The default page of `run_events`.
pub const DEFAULT_EVENTS_PAGE: u32 = 100;
/// The largest page of `run_events`.
pub const MAX_EVENTS_PAGE: u32 = 200;
/// The largest number of steps a [`Run`] carries.
pub const MAX_STEPS: usize = 500;
/// The largest number of documents a [`Run`] names.
pub const MAX_DOCUMENTS: usize = 200;

/// What a port call answers.
pub type ProcessResult<T> = Result<T, ProcessPortError>;

/// Why a port call was refused. Every variant maps to a code the contract declares; the router
/// adds no detail to any of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessPortError {
    /// `404 process.not_found`: no process of this name that the caller may start.
    NotFound,
    /// `422 process.input_invalid`: the input does not fit the trigger's schema. Field paths and
    /// stable codes only, never a value.
    InputInvalid(Vec<FieldIssue>),
    /// `404 process.run_not_found`: no run of this id that the caller may see.
    RunNotFound,
    /// `409 process.run_not_active`: the run has ended.
    RunNotActive,
    /// `403 permission.denied`: the caller sees the run but may not cancel it.
    PermissionDenied,
    /// `400 api.invalid_request`: the cursor is not one this port issued.
    BadCursor,
    /// `503 server.unavailable`: the engine or its store cannot answer now.
    Unavailable,
    /// `500 http.internal`: anything else. The router answers it with no detail.
    Internal,
}

/// The state of a run (the contract's `RunState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RunState {
    /// Running.
    Running,
    /// Waiting on a person, a timer or another organisation's reply.
    Waiting,
    /// Final: completed.
    Completed,
    /// Final: failed.
    Failed,
    /// Final: cancelled.
    Cancelled,
}

impl RunState {
    /// The contract's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Waiting => "waiting",
            RunState::Completed => "completed",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
        }
    }

    /// Parses the contract's spelling; anything else is `None`.
    pub fn parse(raw: &str) -> Option<RunState> {
        match raw {
            "running" => Some(RunState::Running),
            "waiting" => Some(RunState::Waiting),
            "completed" => Some(RunState::Completed),
            "failed" => Some(RunState::Failed),
            "cancelled" => Some(RunState::Cancelled),
            _ => None,
        }
    }

    /// Whether the run has ended (`completed`, `failed` or `cancelled`).
    pub fn is_final(self) -> bool {
        matches!(
            self,
            RunState::Completed | RunState::Failed | RunState::Cancelled
        )
    }
}

/// The state of one step (the contract's `StepState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    /// Not reached yet.
    Pending,
    /// Running now.
    Active,
    /// Waiting.
    Waiting,
    /// Done.
    Completed,
    /// Failed.
    Failed,
    /// Skipped.
    Skipped,
    /// Cancelled.
    Cancelled,
}

impl StepState {
    /// The contract's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            StepState::Pending => "pending",
            StepState::Active => "active",
            StepState::Waiting => "waiting",
            StepState::Completed => "completed",
            StepState::Failed => "failed",
            StepState::Skipped => "skipped",
            StepState::Cancelled => "cancelled",
        }
    }
}

/// One step of a run (the contract's `RunStep`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStep {
    /// The node id (1 to 128 characters).
    pub node: String,
    /// Human label (1 to 140 characters).
    pub label: String,
    /// The step's state.
    pub state: StepState,
    /// When the step started, RFC 3339 UTC.
    pub started_at: Option<String>,
    /// When the step ended, RFC 3339 UTC.
    pub ended_at: Option<String>,
}

/// A document a run touches, by DocType and id only (the contract's `DocumentRef`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentRef {
    /// The DocType's storage name.
    pub doctype: String,
    /// The document id (a canonical UUIDv7).
    pub id: String,
}

/// A run (the contract's `Run`): no variables, no node outputs and no error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// The opaque run id (`^[A-Za-z0-9_-]{16,64}$`).
    pub id: String,
    /// The process's storage name.
    pub process: String,
    /// The process version the run is on.
    pub version: u32,
    /// The run's state.
    pub state: RunState,
    /// The client's own reference; not unique, grants nothing.
    pub business_key: Option<String>,
    /// When the run started, RFC 3339 UTC.
    pub started_at: String,
    /// When the run ended, RFC 3339 UTC; `None` while it runs.
    pub ended_at: Option<String>,
    /// The stable failure code of a `failed` run; `None` otherwise.
    pub failure_code: Option<String>,
    /// The steps, at most [`MAX_STEPS`].
    pub steps: Vec<RunStep>,
    /// The documents the run touches, at most [`MAX_DOCUMENTS`].
    pub documents: Vec<DocumentRef>,
}

/// One line of a run's audit trail (the contract's `RunEvent`): no payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEvent {
    /// 1, 2, 3, ... with no gap.
    pub seq: u64,
    /// When it happened, RFC 3339 UTC.
    pub at: String,
    /// The event kind (`^[a-z][a-z0-9_.]*$`).
    pub kind: String,
    /// The node, when the event belongs to one.
    pub node: Option<String>,
    /// A user id, `system:<name>` or `consumer:<id>`.
    pub actor: Option<String>,
}

/// One process the caller may start (the contract's `ProcessSummary`).
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSummary {
    /// The process's storage name.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The latest published version.
    pub version: u32,
    /// The trigger's JSON Schema subset.
    pub input_schema: Json,
}

/// One page of a list: the items and the opaque cursor of the next page.
#[derive(Clone, Debug, PartialEq)]
pub struct ListPage<T> {
    /// The items, at most the requested limit.
    pub items: Vec<T>,
    /// The cursor of the next page; `None` on the last.
    pub next_cursor: Option<String>,
}

/// The body of `run_start`, already shaped by the route. The starter is never in it: the port
/// takes it from the request's credential.
#[derive(Clone, Debug, PartialEq)]
pub struct RunStart {
    /// The start input (an object of at most 64 KiB serialised); the port validates it against
    /// the trigger's schema.
    pub input: Map<String, Json>,
    /// The client's own reference.
    pub business_key: Option<String>,
}

/// The filters of `run_list`, already shaped by the route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunQuery {
    /// Only runs of this process.
    pub process: Option<String>,
    /// Only runs in this state.
    pub state: Option<RunState>,
    /// Only runs the caller started.
    pub mine: bool,
    /// The cursor of the previous page.
    pub cursor: Option<String>,
    /// The page size, 1 to [`MAX_PAGE`].
    pub limit: u32,
}

/// The process and run operations of one site, for the signed-in caller.
pub trait ProcessPort: Send + Sync {
    /// The processes whose latest published version the caller may start, ordered by name,
    /// filtered by the caller's rights **before** the page is cut.
    fn list_processes<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, ProcessResult<ListPage<ProcessSummary>>>;

    /// Starts a run of `process` as the caller. `ctx.idempotency_key` is the start's source
    /// event: a second start with the same key returns the same run.
    fn start_run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        start: RunStart,
    ) -> BoxFuture<'a, ProcessResult<Run>>;

    /// The runs the caller may see, newest first, filtered by visibility before the page is cut.
    fn list_runs<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: RunQuery,
    ) -> BoxFuture<'a, ProcessResult<ListPage<Run>>>;

    /// One run. A run that does not exist and one the caller may not see are the same
    /// [`ProcessPortError::RunNotFound`].
    fn get_run<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str)
    -> BoxFuture<'a, ProcessResult<Run>>;

    /// The events of a run with `seq` greater than `after`, oldest first, at most `limit`.
    fn run_events<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        after: u64,
        limit: u32,
    ) -> BoxFuture<'a, ProcessResult<Vec<RunEvent>>>;

    /// Cancels a run as the caller. `reason` goes to the audit trail only.
    fn cancel_run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, ProcessResult<Run>>;
}

// ------------------------------------------------------------------------------------------
// The in-memory fake

/// A process the fake knows. The fake's "roles" are plain user ids: an empty `starters` list
/// means any signed-in caller.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessDef {
    /// The storage name.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The latest published version.
    pub version: u32,
    /// The callers (user ids) the trigger admits; empty means any signed-in caller.
    pub starters: BTreeSet<String>,
    /// The input fields the start requires.
    pub required: Vec<String>,
    /// The input fields the start accepts (a field outside it is invalid).
    pub allowed: Vec<String>,
}

impl ProcessDef {
    /// A process any signed-in caller may start, taking the given input fields (all required).
    pub fn open(name: &str, label: &str, fields: &[&str]) -> ProcessDef {
        ProcessDef {
            name: name.to_owned(),
            label: label.to_owned(),
            version: 1,
            starters: BTreeSet::new(),
            required: fields.iter().map(|f| (*f).to_owned()).collect(),
            allowed: fields.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    /// The same process, startable only by these users.
    pub fn only_for(mut self, users: &[&str]) -> ProcessDef {
        self.starters = users.iter().map(|u| (*u).to_owned()).collect();
        self
    }

    fn input_schema(&self) -> Json {
        let mut properties = Map::new();
        for field in &self.allowed {
            properties.insert(field.clone(), json!({"type": "string", "maxLength": 256}));
        }
        json!({
            "type": "object",
            "required": self.required,
            "properties": Json::Object(properties),
            "additionalProperties": false,
        })
    }
}

/// One refused or accepted write the fake audited: who, which operation, and the rule that
/// refused. **Never a value** (no input, no reason, no key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// The site the write was for.
    pub site: String,
    /// The acting user.
    pub actor: String,
    /// `run_start` or `run_cancel`.
    pub operation: &'static str,
    /// The subject: a process name or a run id.
    pub subject: String,
    /// `ok` or the refusing code (`process.not_found`, `process.input_invalid`,
    /// `process.run_not_found`, `process.run_not_active`, `permission.denied`).
    pub outcome: &'static str,
}

struct StoredRun {
    run: Run,
    starter: String,
    parties: BTreeSet<String>,
    events: Vec<RunEvent>,
}

#[derive(Default)]
struct SiteState {
    processes: BTreeMap<String, ProcessDef>,
    supervisors: BTreeSet<String>,
    runs: Vec<StoredRun>,
    starts: HashMap<String, String>,
}

#[derive(Default)]
struct State {
    sites: HashMap<String, SiteState>,
    audit: Vec<AuditEntry>,
    clock: u64,
    next_run: u64,
}

/// The in-memory [`ProcessPort`]: per-site processes and runs, the contract's visibility and
/// cancel rules, and an audit list. The user of a call is the credential's token itself (the
/// fake has no sessions), so `alice-token` is the user `alice-token`.
#[derive(Default)]
pub struct MemoryProcessPort {
    state: Mutex<State>,
}

impl MemoryProcessPort {
    /// An empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The mutex guards plain maps; a panic elsewhere cannot leave them inconsistent.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Publishes `def` on `site`.
    pub fn define(&self, site: &str, def: ProcessDef) {
        let mut state = self.lock();
        state
            .sites
            .entry(site.to_owned())
            .or_default()
            .processes
            .insert(def.name.clone(), def);
    }

    /// Gives `user` the platform permission `process.supervise` on `site`.
    pub fn grant_supervise(&self, site: &str, user: &str) {
        let mut state = self.lock();
        state
            .sites
            .entry(site.to_owned())
            .or_default()
            .supervisors
            .insert(user.to_owned());
    }

    /// Makes `user` an assignee of one of the run's tasks (so the run is visible to them).
    pub fn add_party(&self, site: &str, run_id: &str, user: &str) {
        let mut state = self.lock();
        if let Some(stored) = state
            .sites
            .entry(site.to_owned())
            .or_default()
            .runs
            .iter_mut()
            .find(|s| s.run.id == run_id)
        {
            stored.parties.insert(user.to_owned());
        }
    }

    /// Moves a run to `state` (a test helper standing for the engine finishing it).
    pub fn force_state(&self, site: &str, run_id: &str, run_state: RunState) {
        let mut guard = self.lock();
        let state = &mut *guard;
        state.clock += 1;
        let at = stamp(state.clock);
        if let Some(stored) = state
            .sites
            .entry(site.to_owned())
            .or_default()
            .runs
            .iter_mut()
            .find(|s| s.run.id == run_id)
        {
            stored.run.state = run_state;
            if run_state.is_final() {
                stored.run.ended_at = Some(at);
            }
            if run_state == RunState::Failed {
                stored.run.failure_code = Some("bpm.node_failed".to_owned());
            }
        }
    }

    /// The writes the fake audited, oldest first.
    pub fn audit(&self) -> Vec<AuditEntry> {
        self.lock().audit.clone()
    }

    /// How many runs `site` holds (a test helper).
    pub fn run_count(&self, site: &str) -> usize {
        self.lock().sites.get(site).map_or(0, |s| s.runs.len())
    }
}

/// A deterministic RFC 3339 timestamp: the fake reads no clock.
fn stamp(tick: u64) -> String {
    let hour = (tick / 3600) % 24;
    let minute = (tick / 60) % 60;
    let second = tick % 60;
    format!("2026-11-01T{hour:02}:{minute:02}:{second:02}Z")
}

/// The caller's user id, or a refusal: a call without a credential sees nothing and does nothing
/// (fail closed).
fn caller(ctx: &RequestCtx) -> ProcessResult<String> {
    match &ctx.credential {
        Some(credential) if !credential.token.is_empty() => Ok(credential.token.clone()),
        _ => Err(ProcessPortError::PermissionDenied),
    }
}

fn visible(site: &SiteState, stored: &StoredRun, user: &str) -> bool {
    stored.starter == user || stored.parties.contains(user) || site.supervisors.contains(user)
}

/// The offset a cursor stands for. The fake's cursor is `c<offset>`.
fn parse_cursor(cursor: Option<&str>) -> ProcessResult<usize> {
    match cursor {
        None => Ok(0),
        Some(raw) => raw
            .strip_prefix('c')
            .and_then(|digits| digits.parse::<usize>().ok())
            .ok_or(ProcessPortError::BadCursor),
    }
}

fn page_of<T: Clone>(all: &[T], offset: usize, limit: u32) -> ListPage<T> {
    let limit = limit as usize;
    let items: Vec<T> = all.iter().skip(offset).take(limit).cloned().collect();
    let next = offset.saturating_add(limit);
    let next_cursor = if next < all.len() {
        Some(format!("c{next}"))
    } else {
        None
    };
    ListPage { items, next_cursor }
}

fn input_issues(def: &ProcessDef, input: &Map<String, Json>) -> Vec<FieldIssue> {
    let mut issues = Vec::new();
    for field in &def.required {
        if !input.contains_key(field) {
            issues.push(FieldIssue::new(
                field.clone(),
                "process.input_required",
                "This field is required.",
            ));
        }
    }
    for key in input.keys() {
        if !def.allowed.contains(key) {
            issues.push(FieldIssue::new(
                key.clone(),
                "process.input_unknown_field",
                "This field is not accepted.",
            ));
        }
    }
    issues
}

impl ProcessPort for MemoryProcessPort {
    fn list_processes<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, ProcessResult<ListPage<ProcessSummary>>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let offset = parse_cursor(cursor)?;
            let state = self.lock();
            let all: Vec<ProcessSummary> = state
                .sites
                .get(ctx.site.as_str())
                .map(|site| {
                    site.processes
                        .values()
                        .filter(|def| def.starters.is_empty() || def.starters.contains(&user))
                        .map(|def| ProcessSummary {
                            name: def.name.clone(),
                            label: def.label.clone(),
                            version: def.version,
                            input_schema: def.input_schema(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(page_of(&all, offset, limit))
        })
    }

    fn start_run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        start: RunStart,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let site_name = ctx.site.as_str().to_owned();
            let mut guard = self.lock();
            let state = &mut *guard;
            let note = |outcome: &'static str, log: &mut Vec<AuditEntry>| {
                log.push(AuditEntry {
                    site: site_name.clone(),
                    actor: user.clone(),
                    operation: "run_start",
                    subject: process.to_owned(),
                    outcome,
                });
            };
            let site = state.sites.entry(site_name.clone()).or_default();
            let Some(def) = site
                .processes
                .get(process)
                .filter(|def| def.starters.is_empty() || def.starters.contains(&user))
                .cloned()
            else {
                note("process.not_found", &mut state.audit);
                return Err(ProcessPortError::NotFound);
            };
            let issues = input_issues(&def, &start.input);
            if !issues.is_empty() {
                note("process.input_invalid", &mut state.audit);
                return Err(ProcessPortError::InputInvalid(issues));
            }
            // The key is the start's source event: the same caller and key never start twice.
            let source = ctx
                .idempotency_key
                .as_ref()
                .map(|key| format!("{user}\n{process}\n{key}"));
            if let Some(source) = &source
                && let Some(run_id) = site.starts.get(source)
                && let Some(stored) = site.runs.iter().find(|s| &s.run.id == run_id)
            {
                return Ok(stored.run.clone());
            }
            state.clock += 1;
            state.next_run += 1;
            let at = stamp(state.clock);
            let run = Run {
                id: format!("run_{:020}", state.next_run),
                process: def.name.clone(),
                version: def.version,
                state: RunState::Running,
                business_key: start.business_key.clone(),
                started_at: at.clone(),
                ended_at: None,
                failure_code: None,
                steps: vec![RunStep {
                    node: "start".to_owned(),
                    label: "Start".to_owned(),
                    state: StepState::Active,
                    started_at: Some(at.clone()),
                    ended_at: None,
                }],
                documents: Vec::new(),
            };
            let site = state.sites.entry(site_name.clone()).or_default();
            if let Some(source) = source {
                site.starts.insert(source, run.id.clone());
            }
            site.runs.push(StoredRun {
                run: run.clone(),
                starter: user.clone(),
                parties: BTreeSet::new(),
                events: vec![RunEvent {
                    seq: 1,
                    at,
                    kind: "run.started".to_owned(),
                    node: None,
                    actor: Some(user.clone()),
                }],
            });
            note("ok", &mut state.audit);
            Ok(run)
        })
    }

    fn list_runs<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: RunQuery,
    ) -> BoxFuture<'a, ProcessResult<ListPage<Run>>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let offset = parse_cursor(query.cursor.as_deref())?;
            let state = self.lock();
            let mut all: Vec<Run> = Vec::new();
            if let Some(site) = state.sites.get(ctx.site.as_str()) {
                // Newest first; the visibility rule applies before the page is cut.
                for stored in site.runs.iter().rev() {
                    if !visible(site, stored, &user) {
                        continue;
                    }
                    if query.mine && stored.starter != user {
                        continue;
                    }
                    if query
                        .process
                        .as_deref()
                        .is_some_and(|p| p != stored.run.process)
                    {
                        continue;
                    }
                    if query.state.is_some_and(|s| s != stored.run.state) {
                        continue;
                    }
                    all.push(stored.run.clone());
                }
            }
            Ok(page_of(&all, offset, query.limit))
        })
    }

    fn get_run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let state = self.lock();
            state
                .sites
                .get(ctx.site.as_str())
                .and_then(|site| {
                    site.runs
                        .iter()
                        .find(|s| s.run.id == id && visible(site, s, &user))
                })
                .map(|stored| stored.run.clone())
                .ok_or(ProcessPortError::RunNotFound)
        })
    }

    fn run_events<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        after: u64,
        limit: u32,
    ) -> BoxFuture<'a, ProcessResult<Vec<RunEvent>>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let state = self.lock();
            let stored = state
                .sites
                .get(ctx.site.as_str())
                .and_then(|site| {
                    site.runs
                        .iter()
                        .find(|s| s.run.id == id && visible(site, s, &user))
                })
                .ok_or(ProcessPortError::RunNotFound)?;
            Ok(stored
                .events
                .iter()
                .filter(|event| event.seq > after)
                .take(limit as usize)
                .cloned()
                .collect())
        })
    }

    fn cancel_run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        Box::pin(async move {
            // The reason is for the audit trail of a real engine; the fake never stores it.
            let _ = reason;
            let user = caller(ctx)?;
            let site_name = ctx.site.as_str().to_owned();
            let mut guard = self.lock();
            let state = &mut *guard;
            let record = |outcome: &'static str, log: &mut Vec<AuditEntry>| {
                log.push(AuditEntry {
                    site: site_name.clone(),
                    actor: user.clone(),
                    operation: "run_cancel",
                    subject: id.to_owned(),
                    outcome,
                });
            };
            state.clock += 1;
            let at = stamp(state.clock);
            let site = state.sites.entry(site_name.clone()).or_default();
            let supervisor = site.supervisors.contains(&user);
            let Some(stored) = site.runs.iter_mut().find(|s| {
                s.run.id == id && (s.starter == user || s.parties.contains(&user) || supervisor)
            }) else {
                record("process.run_not_found", &mut state.audit);
                return Err(ProcessPortError::RunNotFound);
            };
            if stored.starter != user && !supervisor {
                record("permission.denied", &mut state.audit);
                return Err(ProcessPortError::PermissionDenied);
            }
            if stored.run.state.is_final() {
                record("process.run_not_active", &mut state.audit);
                return Err(ProcessPortError::RunNotActive);
            }
            stored.run.state = RunState::Cancelled;
            stored.run.ended_at = Some(at.clone());
            for step in &mut stored.run.steps {
                if matches!(
                    step.state,
                    StepState::Pending | StepState::Active | StepState::Waiting
                ) {
                    step.state = StepState::Cancelled;
                    step.ended_at = Some(at.clone());
                }
            }
            let seq = stored.events.len() as u64 + 1;
            stored.events.push(RunEvent {
                seq,
                at,
                kind: "run.cancelled".to_owned(),
                node: None,
                actor: Some(user.clone()),
            });
            let run = stored.run.clone();
            record("ok", &mut state.audit);
            Ok(run)
        })
    }
}
