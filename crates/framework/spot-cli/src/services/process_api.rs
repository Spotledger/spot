//! The process and run operations of one site (`process_list`, `run_start`, `run_list`,
//! `run_get`, `run_events`, `run_cancel`; native API 0.12.0, WP-BPM-02.T6): the production
//! [`ProcessPort`] over the engine host of `services/process.rs`.
//!
//! `spot-http` owns the routes and passes the port **no user, no role and no tenant**; this
//! module decides everything else for the caller the credential names:
//!
//! - **start**: a process is listed and startable when its latest published version has a
//!   `manual` or `api` trigger whose roles admit the caller ([`PermissionGate`]); a process the
//!   caller may not start is [`ProcessPortError::NotFound`], like one that does not exist. The
//!   input is checked against the trigger's schema and refused with field paths and fixed codes.
//! - **business keys** are namespaced `<process>:<initiator>:bk:<key>` (or `...:ik:<key>` from the
//!   `Idempotency-Key` without one) before the engine sees them: the case table has a unique
//!   `(tenant, business_key)` index, so no user can collide with or learn another's key. A repeat
//!   by the same initiator is the existing Run.
//! - **visibility**: initiator, a person tied to one of the run's tasks, or a holder of
//!   `process.supervise`. Anything else is [`ProcessPortError::RunNotFound`]. A source that errors
//!   denies. A run carries step states, the documents of its Case file **the caller may read**,
//!   and a stable failure code; never variables, outputs, failure text or a document value.
//! - **cancel**: the initiator or a `process.supervise` holder; everyone else who sees the run is
//!   `permission.denied`. A cancelled run with a compensation in its Case file says
//!   `run.compensation_pending` in its events.
//!
//! Everything that touches the site is behind [`Access`], [`RunDirectory`] and [`AuditSink`]. With
//! no [`RunDirectory`] port (the site builds [`PgRunDirectory`] over the Case table by default; SQL stays in
//! storage crates) `run_list` answers `503`. Audit target `spot::audit::process`: every start and
//! cancel, refusals included; a cancel carries the length of the reason, never its text.
//! Settings `[services.process_api]` (`enabled`); see `docs/running-spot/process_api.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::RecordId;
use spot_bpm_definitions::DefinitionStore;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_documents::{CaseFileStore, Entry};
use spot_bpm_engine::{CaseId, CaseRecord, CaseStore, ProcessDefinition};
use spot_bpm_model::NodeKind;
use spot_bpm_tasks::{Task, TaskFilter, TaskStore};
use spot_bpm_triggers::{PermissionGate, StartGate, StartPermission, TriggerSpec, validate_input};
use spot_core::{Clock, SiteId};
use spot_dag_exec::{Instant, NodeStatus, RunStatus};
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_http::api::Ports;
use spot_http::api::idempotency::{IdempotencyStore, MemoryIdempotencyStore};
use spot_http::api::ports::process::{
    DocumentRef, ListPage, MAX_DOCUMENTS, MAX_STEPS, ProcessPort, ProcessPortError, ProcessResult,
    ProcessSummary, Run, RunEvent, RunQuery, RunStart, RunState, RunStep, StepState,
};
use spot_http::api::router::routes;
use spot_http::api::service::{BoxFuture, FieldIssue, RequestCtx};
use spot_permissions::Principal;
use time::format_description::well_known::Rfc3339;

use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::SiteCtx;
use crate::services::process::{ProcessEngine, ProcessHost};
use crate::services::process_documents::PgAfterCaseFile;
use crate::{Error, Result};

/// Start order: after the process engine (1100) and the node-kind modules.
pub const ORDER: u32 = 1150;

const AUDIT_TARGET: &str = "spot::audit::process";
const SUPERVISE: &str = "process_supervise";
const FAILURE_CODE: &str = "bpm_engine.node_failed";
const ENGINE_ACTOR: &str = "system:process_engine";
const KEY_MAX: usize = 128;
const MAX_ISSUES: usize = 50;
const MAX_ASSIGNED: usize = 1000;

/// `[services.process_api]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module serves the routes at all.
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The authenticated caller of one request.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The user's id as text: the initiator recorded on a Run.
    pub user: String,
    /// The user's id.
    pub user_id: RecordId,
    /// The user's assignments, for trigger roles and the permission engine.
    pub principal: Principal,
}

/// A source that could not answer: always a denial, never an allowance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceUnavailable;

/// What a source answers, or that it could not.
pub type Sourced<'a, T> = BoxFuture<'a, std::result::Result<T, SourceUnavailable>>;

/// Cancels the open tasks of a Case that was cancelled (provided by the task inbox module). A
/// cancelled run takes its tasks with it: none stays in an inbox.
pub trait CaseTaskCanceller: Send + Sync {
    /// Cancels every open task of `case`; a failure is logged by the provider, never raised (the
    /// run is already cancelled).
    fn cancel_case<'a>(&'a self, case: &'a str) -> BoxFuture<'a, ()>;
}

/// Everything the port asks of the site: identity, rights and the ties of tasks to runs.
pub trait Access: Send + Sync {
    /// The caller of `ctx`. A missing, expired or restricted credential is
    /// [`ProcessPortError::PermissionDenied`]; a service that cannot answer is `Unavailable`.
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, ProcessResult<Caller>>;
    /// Whether `principal` holds the platform permission `process.supervise`.
    fn supervises(&self, principal: &Principal) -> bool;
    /// Whether `caller` may read the document (the document engine's check, as the caller).
    fn may_read<'a>(&'a self, c: &'a Caller, doctype: &'a str, id: RecordId)
    -> BoxFuture<'a, bool>;
    /// The cases of the tasks `user` is assigned to (at most 1,000).
    fn assigned_cases<'a>(&'a self, user: &'a str) -> Sourced<'a, Vec<String>>;
    /// Whether `user` is tied to one of `case`'s tasks (assignee, claimer or delegate).
    fn is_party<'a>(&'a self, user: &'a str, case: &'a str) -> Sourced<'a, bool>;
    /// Who decided each completed Decision node of `case`, by node (a person, never the clock).
    /// Nobody by default.
    fn deciders<'a>(&'a self, _case: &'a str) -> Sourced<'a, BTreeMap<String, String>> {
        Box::pin(async { Ok(BTreeMap::new()) })
    }
    /// Cancels the open tasks of `case` after the run was cancelled. Nothing by default.
    fn cancel_tasks<'a>(&'a self, _case: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// The listing question: which cases the viewer may see, newest first. A storage module that
/// provides a [`RunDirectory`] applies the visibility **in the query**, before the cut and any
/// count: `initiator = viewer`, or `case in also_cases`, or everything when `sees_all`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryQuery {
    /// The viewer's user id.
    pub viewer: String,
    /// The cases of the viewer's tasks.
    pub also_cases: Vec<String>,
    /// The viewer holds `process.supervise`.
    pub sees_all: bool,
    /// Only runs of this process.
    pub process: Option<String>,
    /// Only runs of this coarse status (`waiting` is refined by the port).
    pub status: Option<RunStatus>,
    /// Only runs the viewer started.
    pub mine: bool,
    /// The cursor this directory issued.
    pub cursor: Option<String>,
    /// The page size.
    pub limit: u32,
}

/// The visibility-filtered listing of a site's Cases (provided by a storage module). Answers
/// with case ids, newest first; a foreign cursor is [`ProcessPortError::BadCursor`].
pub trait RunDirectory: Send + Sync {
    /// One page for `query`.
    fn page<'a>(
        &'a self,
        query: &'a DirectoryQuery,
    ) -> BoxFuture<'a, ProcessResult<ListPage<String>>>;
}

/// The production [`RunDirectory`]: the Case table of the site, the visibility in the query,
/// newest first, paged by a keyset cursor (`r<micros>.<case>`) this directory issues.
pub struct PgRunDirectory {
    store: spot_bpm_pg::PgCaseStore,
}

impl PgRunDirectory {
    /// The directory over `store` (already scoped to the site's tenant).
    #[must_use]
    pub fn new(store: spot_bpm_pg::PgCaseStore) -> Self {
        Self { store }
    }
}

fn status_text(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn directory_cursor(at: time::OffsetDateTime, id: &str) -> String {
    format!("r{}.{id}", at.unix_timestamp_nanos() / 1000)
}

fn parse_directory_cursor(raw: &str) -> ProcessResult<(time::OffsetDateTime, String)> {
    let bad = || ProcessPortError::BadCursor;
    let rest = raw.strip_prefix('r').ok_or_else(bad)?;
    let (micros, id) = rest.split_once('.').ok_or_else(bad)?;
    let micros: i128 = micros.parse().map_err(|_| bad())?;
    let at = time::OffsetDateTime::from_unix_timestamp_nanos(micros.checked_mul(1000).ok_or_else(bad)?)
        .map_err(|_| bad())?;
    CaseId::new(id).map_err(|_| bad())?;
    Ok((at, id.to_owned()))
}

impl RunDirectory for PgRunDirectory {
    fn page<'a>(
        &'a self,
        query: &'a DirectoryQuery,
    ) -> BoxFuture<'a, ProcessResult<ListPage<String>>> {
        Box::pin(async move {
            let after = query
                .cursor
                .as_deref()
                .map(parse_directory_cursor)
                .transpose()?;
            let limit = query.limit.clamp(1, 200);
            let status = query.status.map(status_text);
            let rows = self
                .store
                .page_visible(&spot_bpm_pg::VisibleCases {
                    viewer: &query.viewer,
                    also_cases: &query.also_cases,
                    sees_all: query.sees_all,
                    process: query.process.as_deref(),
                    status,
                    mine: query.mine,
                    after,
                    limit: limit + 1,
                })
                .await
                .map_err(unavailable)?;
            let more = rows.len() > limit as usize;
            let kept: Vec<_> = rows.into_iter().take(limit as usize).collect();
            let next_cursor = if more {
                kept.last().map(|(id, at)| directory_cursor(*at, id))
            } else {
                None
            };
            Ok(ListPage {
                items: kept.into_iter().map(|(id, _)| id).collect(),
                next_cursor,
            })
        })
    }
}

/// One audited write. **Never a value**: no input, no reason text, no key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// The site.
    pub site: String,
    /// The acting user (`-` when the caller could not be resolved).
    pub actor: String,
    /// `run_start` or `run_cancel`.
    pub operation: &'static str,
    /// A process name or a run id.
    pub subject: String,
    /// `ok` or the refusing code.
    pub outcome: &'static str,
    /// The length of a cancel's reason, in characters.
    pub reason_len: Option<usize>,
}

/// Where audited writes go (the production sink logs one line on `spot::audit::process`).
pub type AuditSink = Arc<dyn Fn(AuditEntry) + Send + Sync>;

fn log_audit(e: AuditEntry) {
    tracing::info!(
        target: AUDIT_TARGET, site = %e.site, who = %e.actor, operation = e.operation,
        subject = %e.subject, outcome = e.outcome, reason_len = e.reason_len.unwrap_or(0),
        "process api"
    );
}

/// What a [`ProcessApi`] is built from.
pub struct Deps {
    /// The site: nothing here is shared between sites, a call for another site is refused.
    pub site: SiteId,
    /// The engine host.
    pub host: Arc<ProcessHost>,
    /// The durable definitions (the process names).
    pub definitions: Arc<dyn DefinitionStore>,
    /// The Case file (documents a run touched).
    pub case_file: Arc<dyn CaseFileStore>,
    /// Identity, rights and task ties.
    pub access: Arc<dyn Access>,
    /// The listing; `None` makes `run_list` unavailable (fail closed).
    pub directory: Option<Arc<dyn RunDirectory>>,
    /// The audit sink.
    pub audit: AuditSink,
    /// The clock: the only source of `now`.
    pub clock: Arc<dyn Clock>,
}

/// The process and run operations of one site.
pub struct ProcessApi(Deps);

fn unavailable<T>(_: T) -> ProcessPortError {
    ProcessPortError::Unavailable
}

/// `at` as RFC 3339 UTC to the second.
fn rfc3339(at: Instant) -> ProcessResult<String> {
    at.to_offset()
        .ok()
        .and_then(|t| t.replace_nanosecond(0).ok())
        .and_then(|t| t.format(&Rfc3339).ok())
        .ok_or(ProcessPortError::Internal)
}

/// The engine key of a start, namespaced by process and initiator.
#[must_use]
pub fn engine_key(process: &str, user: &str, bk: Option<&str>, ik: Option<&str>) -> Option<String> {
    match (bk, ik) {
        (Some(key), _) => Some(format!("{process}:{user}:bk:{key}")),
        (None, Some(key)) => Some(format!("{process}:{user}:ik:{key}")),
        (None, None) => None,
    }
}

/// The caller's own business key out of an engine key, when the start carried one.
#[must_use]
pub fn display_key(process: &str, user: Option<&str>, engine_key: &str) -> Option<String> {
    let prefix = format!("{process}:{}:bk:", user?);
    let rest = engine_key.strip_prefix(prefix.as_str())?;
    (!rest.is_empty() && rest.chars().count() <= KEY_MAX).then(|| rest.to_owned())
}

/// The issues of `validate_input`'s problems: a path and a fixed code and sentence, never text
/// that could carry a value.
fn issues_of(problems: &[String]) -> Vec<FieldIssue> {
    let issue = |problem: &String| {
        let pointer = problem.split(": ").next().unwrap_or("");
        let path = pointer.trim_start_matches('/').replace('/', ".");
        let (code, text) = if problem.contains("required and missing") {
            ("process.input_required", "This field is required.")
        } else if problem.contains("not an allowed field") {
            ("process.input_unknown_field", "This field is not accepted.")
        } else {
            ("process.input_invalid", "This value is not valid.")
        };
        let field = if path.is_empty() {
            "input"
        } else {
            path.as_str()
        };
        FieldIssue::new(field, code, text)
    };
    problems.iter().take(MAX_ISSUES).map(issue).collect()
}

/// The first manual or API trigger of `definition` whose roles admit `principal`. A trigger that
/// does not build is not startable (fail closed).
fn start_spec(definition: &ProcessDefinition, principal: &Principal) -> Option<TriggerSpec> {
    let gate = PermissionGate::roles_only();
    let app = &definition.version.app;
    definition.version.nodes.iter().find_map(|node| {
        let NodeKind::Trigger(config) = &node.kind else {
            return None;
        };
        let spec = TriggerSpec::from_config(definition.name(), app, &node.id, config).ok()?;
        let manual_or_api = matches!(spec, TriggerSpec::Manual(_) | TriggerSpec::Api(_));
        let request = StartPermission {
            principal,
            spec: &spec,
            doc: None,
        };
        (manual_or_api && gate.may_start(&request)).then_some(spec)
    })
}

/// The schema a trigger declares, or the open object when it declares none (the model carries
/// no schema yet).
fn schema_of(spec: &TriggerSpec) -> Json {
    spec.input_schema()
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}))
}

/// A list cursor is the last process name served (`[a-z][a-z0-9_]*`, at most 64).
fn name_cursor(cursor: Option<&str>) -> ProcessResult<Option<String>> {
    let Some(raw) = cursor else { return Ok(None) };
    let first = raw.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    let rest = raw
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if first && rest && raw.len() <= 64 {
        Ok(Some(raw.to_owned()))
    } else {
        Err(ProcessPortError::BadCursor)
    }
}

/// `waiting` while nothing is ready and a timer or a child holds the run.
fn run_state(record: &CaseRecord) -> RunState {
    match record.run.status {
        RunStatus::Completed => RunState::Completed,
        RunStatus::Failed => RunState::Failed,
        RunStatus::Cancelled => RunState::Cancelled,
        RunStatus::Running => {
            let nodes = &record.run.nodes;
            let ready = nodes.values().any(|n| n.status == NodeStatus::Ready);
            if !ready && (!record.timers.is_empty() || !record.children.is_empty()) {
                RunState::Waiting
            } else {
                RunState::Running
            }
        }
    }
}

fn steps_of(record: &CaseRecord) -> ProcessResult<Vec<RunStep>> {
    let cancelled = record.run.status == RunStatus::Cancelled;
    let stamp = |at: Option<Instant>| at.map(rfc3339).transpose();
    let mut steps = Vec::new();
    for (id, node) in record.run.nodes.iter().take(MAX_STEPS) {
        let state = match node.status {
            NodeStatus::Pending | NodeStatus::Ready | NodeStatus::Running if cancelled => {
                StepState::Cancelled
            }
            NodeStatus::Pending | NodeStatus::Ready => StepState::Pending,
            NodeStatus::Running if record.timers.contains_key(id) => StepState::Waiting,
            NodeStatus::Running => StepState::Active,
            NodeStatus::Done => StepState::Completed,
            NodeStatus::Skipped => StepState::Skipped,
            NodeStatus::Failed => StepState::Failed,
        };
        steps.push(RunStep {
            node: id.as_str().to_owned(),
            label: id.as_str().to_owned(),
            state,
            started_at: stamp(node.started_at)?,
            ended_at: stamp(node.ended_at)?,
        });
    }
    Ok(steps)
}

/// One event before numbering: when, a rank for events that share an instant, kind, node, actor.
type Raw = (Instant, u8, &'static str, Option<String>, Option<String>);

/// The events a stored Case implies, oldest first, numbered from 1. Derived, so the same Case
/// gives the same trail.
fn events_of(record: &CaseRecord, compensation_pending: bool) -> Vec<RunEvent> {
    let run = &record.run;
    let user = record
        .initiator
        .clone()
        .filter(|u| !u.is_empty() && u.len() <= KEY_MAX);
    let mut raw: Vec<Raw> = vec![(run.started_at, 0, "run.started", None, user)];
    for (id, node) in &run.nodes {
        let (node_id, actor) = (Some(id.as_str().to_owned()), Some(ENGINE_ACTOR.to_owned()));
        if let Some(at) = node.started_at {
            raw.push((at, 1, "step.started", node_id.clone(), actor.clone()));
        }
        if let Some(at) = node.ended_at {
            let kind = match node.status {
                NodeStatus::Done => "step.completed",
                NodeStatus::Failed => "step.failed",
                NodeStatus::Skipped => "step.skipped",
                _ => "step.cancelled",
            };
            raw.push((at, 2, kind, node_id, actor));
        }
    }
    let end = run.ended_at.unwrap_or(run.started_at);
    match run.status {
        RunStatus::Running => {}
        RunStatus::Completed => raw.push((end, 3, "run.completed", None, None)),
        RunStatus::Failed => raw.push((end, 3, "run.failed", None, None)),
        RunStatus::Cancelled => {
            raw.push((end, 3, "run.cancelled", None, None));
            if compensation_pending {
                raw.push((end, 4, "run.compensation_pending", None, None));
            }
        }
    }
    raw.sort_by(|a, b| (a.0, a.1, &a.3).cmp(&(b.0, b.1, &b.3)));
    let numbered = raw.into_iter().zip(1_u64..);
    numbered
        .filter_map(|((at, _, kind, node, actor), seq)| {
            let at = rfc3339(at).ok()?;
            Some(RunEvent {
                seq,
                at,
                kind: kind.to_owned(),
                node,
                actor,
            })
        })
        .collect()
}

fn outcome_of(error: &ProcessPortError) -> &'static str {
    match error {
        ProcessPortError::NotFound => "process.not_found",
        ProcessPortError::InputInvalid(_) => "process.input_invalid",
        ProcessPortError::RunNotFound => "process.run_not_found",
        ProcessPortError::RunNotActive => "process.run_not_active",
        ProcessPortError::PermissionDenied => "permission.denied",
        ProcessPortError::BadCursor => "api.invalid_request",
        ProcessPortError::Unavailable => "server.unavailable",
        ProcessPortError::Internal => "http.internal",
    }
}

impl ProcessApi {
    /// The port of one site.
    #[must_use]
    pub fn new(deps: Deps) -> Self {
        Self(deps)
    }

    fn engine(&self) -> ProcessResult<Arc<ProcessEngine>> {
        self.0.host.engine().map_err(unavailable)
    }

    /// The caller of `ctx`, for this site only.
    async fn caller(&self, ctx: &RequestCtx) -> ProcessResult<Caller> {
        if ctx.site.as_str() != self.0.site.as_str() {
            return Err(ProcessPortError::PermissionDenied);
        }
        self.0.access.caller(ctx).await
    }

    fn audit<T>(
        &self,
        who: &str,
        operation: &'static str,
        subject: &str,
        done: &ProcessResult<T>,
        reason_len: Option<usize>,
    ) {
        (self.0.audit)(AuditEntry {
            site: self.0.site.as_str().to_owned(),
            actor: who.to_owned(),
            operation,
            subject: subject.to_owned(),
            outcome: done.as_ref().err().map_or("ok", outcome_of),
            reason_len,
        });
    }

    /// Whether `caller` sees `record`: initiator, a supervisor or a task party. A source that
    /// errors denies.
    async fn sees(&self, caller: &Caller, sees_all: bool, record: &CaseRecord) -> bool {
        if sees_all || record.initiator.as_deref() == Some(caller.user.as_str()) {
            return true;
        }
        let tie = self
            .0
            .access
            .is_party(&caller.user, record.run.id.as_str())
            .await;
        matches!(tie, Ok(true))
    }

    /// The Case file of a case; unreadable is empty (fail closed).
    fn entries(&self, case: &CaseId) -> Vec<Entry> {
        let store = Arc::clone(&self.0.case_file);
        let multi = tokio::runtime::Handle::try_current()
            .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
        let read = || store.entries(case);
        let result = if multi {
            tokio::task::block_in_place(read)
        } else {
            read()
        };
        result.unwrap_or_else(|_| {
            tracing::warn!(target: AUDIT_TARGET, "a Case file could not be read");
            Vec::new()
        })
    }

    /// The documents of a run the caller may read, by DocType and id.
    async fn documents(&self, caller: &Caller, case: &CaseId) -> Vec<DocumentRef> {
        let entries = self.entries(case);
        let pairs: BTreeSet<(String, RecordId)> = entries
            .into_iter()
            .map(|e| (e.doctype.as_str().to_owned(), e.document))
            .collect();
        let mut documents = Vec::new();
        for (doctype, id) in pairs.into_iter().take(MAX_DOCUMENTS) {
            if self.0.access.may_read(caller, &doctype, id).await {
                documents.push(DocumentRef {
                    doctype,
                    id: id.to_string(),
                });
            }
        }
        documents
    }

    async fn view(&self, caller: &Caller, record: &CaseRecord) -> ProcessResult<Run> {
        let run = &record.run;
        let state = run_state(record);
        let ended_at = run.ended_at.map(rfc3339).transpose()?;
        Ok(Run {
            id: run.id.as_str().to_owned(),
            process: run.dag_key.clone(),
            version: run.dag_version,
            state,
            business_key: display_key(&run.dag_key, record.initiator.as_deref(), &run.key),
            started_at: rfc3339(run.started_at)?,
            ended_at,
            failure_code: (state == RunState::Failed).then(|| FAILURE_CODE.to_owned()),
            steps: steps_of(record)?,
            documents: self.documents(caller, &run.id).await,
        })
    }

    /// The caller and the stored Case of `id`; [`ProcessPortError::RunNotFound`] when the Case
    /// does not exist or the caller may not see it. Also whether the caller supervises.
    async fn visible_case(
        &self,
        ctx: &RequestCtx,
        id: &str,
    ) -> ProcessResult<(Caller, bool, CaseRecord)> {
        let caller = self.caller(ctx).await?;
        let case = CaseId::new(id).map_err(|_| ProcessPortError::RunNotFound)?;
        let loaded = self
            .engine()?
            .store()
            .load(&case)
            .await
            .map_err(unavailable)?;
        let record = loaded.ok_or(ProcessPortError::RunNotFound)?;
        let sees_all = self.0.access.supervises(&caller.principal);
        if !self.sees(&caller, sees_all, &record).await {
            return Err(ProcessPortError::RunNotFound);
        }
        Ok((caller, sees_all, record))
    }

    async fn start(
        &self,
        ctx: &RequestCtx,
        caller: &Caller,
        process: &str,
        start: RunStart,
    ) -> ProcessResult<Run> {
        let engine = self.engine()?;
        let definition = engine
            .processes()
            .latest(process)
            .ok_or(ProcessPortError::NotFound)?;
        let spec = start_spec(&definition, &caller.principal).ok_or(ProcessPortError::NotFound)?;
        let input = Json::Object(start.input);
        let problems = validate_input(&schema_of(&spec), &input);
        if !problems.is_empty() {
            return Err(ProcessPortError::InputInvalid(issues_of(&problems)));
        }
        let (bk, ik) = (
            start.business_key.as_deref(),
            ctx.idempotency_key.as_deref(),
        );
        let key = engine_key(process, &caller.user, bk, ik).ok_or(ProcessPortError::Internal)?;
        let store = engine.store();
        let mut record = store.find_by_key(&key).await.map_err(unavailable)?;
        if record.is_none() {
            let now = self.0.clock.now_utc();
            let user = Some(caller.user.clone());
            let started = self
                .0
                .host
                .start_run(process, None, input, user, &key, now)
                .await;
            // A racing start of the same key may have won: the key's Run is then the answer.
            record = match started {
                Ok(case) => store.load(&case).await.map_err(unavailable)?,
                Err(_) => store.find_by_key(&key).await.map_err(unavailable)?,
            };
        }
        let record = record.ok_or(ProcessPortError::Unavailable)?;
        self.view(caller, &record).await
    }

    async fn cancel(
        &self,
        caller: &Caller,
        sees_all: bool,
        record: &CaseRecord,
    ) -> ProcessResult<Run> {
        // Seeing the run through a task is not the right to cancel it.
        if record.initiator.as_deref() != Some(caller.user.as_str()) && !sees_all {
            return Err(ProcessPortError::PermissionDenied);
        }
        if record.run.status != RunStatus::Running {
            return Err(ProcessPortError::RunNotActive);
        }
        let now = self.0.clock.now_utc();
        let cancelled = self
            .engine()?
            .cancel(&record.run.id, now)
            .await
            .map_err(unavailable)?;
        self.0.access.cancel_tasks(cancelled.run.id.as_str()).await;
        self.view(caller, &cancelled).await
    }
}

impl ProcessPort for ProcessApi {
    fn list_processes<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, ProcessResult<ListPage<ProcessSummary>>> {
        Box::pin(async move {
            let caller = self.caller(ctx).await?;
            let after = name_cursor(cursor)?;
            let names = self.0.definitions.names().await.map_err(unavailable)?;
            let engine = self.engine()?;
            let mut items: Vec<ProcessSummary> = Vec::new();
            let mut next_cursor = None;
            for name in names {
                if after.as_deref().is_some_and(|a| name.as_str() <= a) {
                    continue;
                }
                let Some(definition) = engine.processes().latest(&name) else {
                    continue;
                };
                let Some(spec) = start_spec(&definition, &caller.principal) else {
                    continue;
                };
                if items.len() >= limit as usize {
                    next_cursor = items.last().map(|s| s.name.clone());
                    break;
                }
                let title = definition.version.title.clone().filter(|t| !t.is_empty());
                items.push(ProcessSummary {
                    label: title
                        .unwrap_or_else(|| name.clone())
                        .chars()
                        .take(140)
                        .collect(),
                    name,
                    version: definition.number(),
                    input_schema: schema_of(&spec),
                });
            }
            Ok(ListPage { items, next_cursor })
        })
    }

    fn start_run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        start: RunStart,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        Box::pin(async move {
            let caller = match self.caller(ctx).await {
                Ok(caller) => caller,
                Err(error) => {
                    self.audit(
                        "-",
                        "run_start",
                        process,
                        &Err::<(), _>(error.clone()),
                        None,
                    );
                    return Err(error);
                }
            };
            let result = self.start(ctx, &caller, process, start).await;
            self.audit(&caller.user, "run_start", process, &result, None);
            result
        })
    }

    fn list_runs<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: RunQuery,
    ) -> BoxFuture<'a, ProcessResult<ListPage<Run>>> {
        Box::pin(async move {
            let caller = self.caller(ctx).await?;
            let sees_all = self.0.access.supervises(&caller.principal);
            // A task store that errors narrows the list to the caller's own runs (fail closed).
            let also_cases = if sees_all || query.mine {
                Vec::new()
            } else {
                self.0
                    .access
                    .assigned_cases(&caller.user)
                    .await
                    .unwrap_or_default()
            };
            let directory = self
                .0
                .directory
                .as_ref()
                .ok_or(ProcessPortError::Unavailable)?;
            let status = query.state.map(|state| match state {
                RunState::Running | RunState::Waiting => RunStatus::Running,
                RunState::Completed => RunStatus::Completed,
                RunState::Failed => RunStatus::Failed,
                RunState::Cancelled => RunStatus::Cancelled,
            });
            let asked = DirectoryQuery {
                viewer: caller.user.clone(),
                also_cases,
                sees_all,
                process: query.process.clone(),
                status,
                mine: query.mine,
                cursor: query.cursor.clone(),
                limit: query.limit,
            };
            let page = directory.page(&asked).await?;
            let engine = self.engine()?;
            let mut items = Vec::new();
            for id in page.items.iter().take(query.limit as usize) {
                let Ok(case) = CaseId::new(id) else { continue };
                let Some(record) = engine.store().load(&case).await.map_err(unavailable)? else {
                    continue;
                };
                // The directory filters; the port checks again, so a directory fault shows nothing.
                let mine = record.initiator.as_deref() == Some(caller.user.as_str());
                if !self.sees(&caller, sees_all, &record).await
                    || query
                        .process
                        .as_deref()
                        .is_some_and(|p| p != record.run.dag_key)
                    || query.state.is_some_and(|s| s != run_state(&record))
                    || (query.mine && !mine)
                {
                    continue;
                }
                items.push(self.view(&caller, &record).await?);
            }
            Ok(ListPage {
                items,
                next_cursor: page.next_cursor,
            })
        })
    }

    fn get_run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        Box::pin(async move {
            let (caller, _, record) = self.visible_case(ctx, id).await?;
            self.view(&caller, &record).await
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
            let (_, _, record) = self.visible_case(ctx, id).await?;
            let cancelled = record.run.status == RunStatus::Cancelled;
            let compensation = cancelled
                && self
                    .entries(&record.run.id)
                    .iter()
                    .any(|e| e.compensation.is_some());
            let mut events = events_of(&record, compensation);
            // A Decision step was completed by a person, not by the engine: the event names the
            // person who decided (the task's holder), so the trail shows who approved.
            let deciders = self
                .0
                .access
                .deciders(record.run.id.as_str())
                .await
                .unwrap_or_default();
            for event in &mut events {
                if event.kind == "step.completed"
                    && let Some(who) = event.node.as_ref().and_then(|n| deciders.get(n))
                    && who.len() <= KEY_MAX
                {
                    event.actor = Some(who.clone());
                }
            }
            Ok(events
                .into_iter()
                .filter(|e| e.seq > after)
                .take(limit as usize)
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
            let (who, result) = match self.visible_case(ctx, id).await {
                Ok((caller, sees_all, record)) => (
                    caller.user.clone(),
                    self.cancel(&caller, sees_all, &record).await,
                ),
                Err(error) => {
                    let known = self.caller(ctx).await.map(|c| c.user);
                    (known.unwrap_or_else(|_| "-".to_owned()), Err(error))
                }
            };
            // The reason is audited by length only: its text is never logged.
            let length = Some(reason.chars().count());
            self.audit(&who, "run_cancel", id, &result, length);
            result
        })
    }
}

/// The production [`Access`]: the live sign-in service, the permission snapshot, the document
/// engine and, when a module provides one, the site's task store.
struct LiveAccess {
    live: Arc<SiteLive>,
    site: SiteId,
    clock: Arc<dyn Clock>,
    tasks: Option<Arc<dyn TaskStore>>,
    canceller: Option<Arc<dyn CaseTaskCanceller>>,
}

/// Lists tasks on a plain thread (the task store's bridge refuses a runtime thread); with no
/// task store nobody is tied to any run.
async fn list_tasks(
    store: &Option<Arc<dyn TaskStore>>,
    filter: TaskFilter,
) -> std::result::Result<Vec<Task>, SourceUnavailable> {
    let Some(store) = store.clone() else {
        return Ok(Vec::new());
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let work = move || {
        let _ = tx.send(store.list(&filter));
    };
    std::thread::Builder::new()
        .spawn(work)
        .map_err(|_| SourceUnavailable)?;
    rx.await
        .map_err(|_| SourceUnavailable)?
        .map_err(|_| SourceUnavailable)
}

impl Access for LiveAccess {
    fn deciders<'a>(&'a self, case: &'a str) -> Sourced<'a, BTreeMap<String, String>> {
        Box::pin(async move {
            let filter = TaskFilter {
                case: Some(case.to_owned()),
                ..Default::default()
            };
            let tasks = list_tasks(&self.tasks, filter).await?;
            Ok(tasks
                .into_iter()
                .filter(|t| {
                    t.status == spot_bpm_tasks::TaskStatus::Completed && !t.auto_completed
                })
                .filter_map(|t| t.claimed_by.map(|who| (t.node, who)))
                .collect())
        })
    }

    fn cancel_tasks<'a>(&'a self, case: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(canceller) = &self.canceller {
                canceller.cancel_case(case).await;
            }
        })
    }

    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, ProcessResult<Caller>> {
        Box::pin(async move {
            let auth = self.live.auth().ok_or(ProcessPortError::Unavailable)?;
            let denied = |_| ProcessPortError::PermissionDenied;
            let who = auth
                .authenticate_on(ctx, PathClass::Api)
                .await
                .map_err(denied)?;
            if auth.is_restricted(&who) {
                return Err(ProcessPortError::PermissionDenied);
            }
            let principals = self
                .live
                .principals()
                .ok_or(ProcessPortError::Unavailable)?;
            let principal = DocPrincipals::resolve(&*principals, &who.user_id).await;
            Ok(Caller {
                user: who.user_id.to_string(),
                user_id: who.user_id,
                principal,
            })
        })
    }

    fn supervises(&self, principal: &Principal) -> bool {
        let served = self.live.served();
        served.is_some_and(|s| s.decision.app_permission(principal, SUPERVISE).allowed)
    }

    fn may_read<'a>(
        &'a self,
        c: &'a Caller,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Some(engine) = self.live.engine() else {
                return false;
            };
            let inv = Invocation {
                site: self.site.clone(),
                actor: Actor::User {
                    id: c.user_id,
                    via: None,
                },
                command_id: None,
                language: None,
                now: self.clock.now_utc(),
            };
            engine.load(&inv, doctype, id).await.is_ok()
        })
    }

    fn assigned_cases<'a>(&'a self, user: &'a str) -> Sourced<'a, Vec<String>> {
        Box::pin(async move {
            let assignees = Some(vec![user.to_owned()]);
            let tasks = list_tasks(
                &self.tasks,
                TaskFilter {
                    assignees,
                    ..Default::default()
                },
            );
            let cases: BTreeSet<String> = tasks.await?.into_iter().map(|t| t.case).collect();
            Ok(cases.into_iter().take(MAX_ASSIGNED).collect())
        })
    }

    fn is_party<'a>(&'a self, user: &'a str, case: &'a str) -> Sourced<'a, bool> {
        Box::pin(async move {
            let filter = TaskFilter {
                case: Some(case.to_owned()),
                ..Default::default()
            };
            let tasks = list_tasks(&self.tasks, filter).await?;
            Ok(tasks.iter().any(|t| {
                t.assignees.contains(user)
                    || t.claimed_by.as_deref() == Some(user)
                    || t.delegation.as_ref().is_some_and(|d| d.to == user)
            }))
        })
    }
}

/// What a site's first build made, so a reload reaches the same port and the same durable
/// Idempotency-Key store.
struct Built {
    port: Arc<ProcessApi>,
    idempotency: Arc<dyn IdempotencyStore>,
}

/// The ports of the sites built so far (a reload must reach the same one).
fn built() -> &'static Mutex<BTreeMap<String, Built>> {
    static BUILT: OnceLock<Mutex<BTreeMap<String, Built>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// `[services.idempotency]` as this module reads it: **leniently** (no `deny_unknown_fields`;
/// the host owns the rest of the table, the retention keys included) and only the one switch the
/// host also honours. The dev-site switch is the host's alone and a hosted module cannot see it,
/// so only `allow_memory_idempotency = true` selects memory here.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct IdempotencySwitch {
    /// Keep the Idempotency-Keys of the process routes in memory (they do not survive a restart,
    /// Recorded security finding).
    pub allow_memory_idempotency: bool,
}

/// The least time a pending reservation outlives its request.
const MIN_IDEMPOTENCY_LEASE: Duration = Duration::from_secs(120);

/// The durable Idempotency-Key store of the process routes: the host's own table
/// (`spot_idempotency_key`) on the site's pool, the standard retention (24 h, capped at 7 d) and
/// a lease of `max(2 * request_timeout, 120 s)`. The host's prune service deletes the expired
/// rows of this table, so no prune is added here.
///
/// # Errors
/// The store cannot be configured, or its table cannot be created; the message names the site
/// and a recorded security finding, never a key or a body.
pub async fn durable_idempotency_store(
    site: &str,
    backend: &spot_postgres::PgBackend,
    clock: Arc<dyn Clock>,
    request_timeout: Duration,
) -> Result<Arc<dyn IdempotencyStore>> {
    let lease = (request_timeout * 2).max(MIN_IDEMPOTENCY_LEASE);
    let config = spot_idempotency_pg::PgIdempotencyConfig::new(
        lease,
        spot_idempotency_pg::RetentionPolicy::standard(),
    );
    let store = spot_idempotency_pg::PgIdempotencyStore::new(backend.pool(), clock, config)
        .map_err(|_| {
            Error(format!(
                "site `{site}`: the idempotency store of the process routes cannot start"
            ))
        })?;
    store.prepare().await.map_err(|_| {
        Error(format!(
            "site `{site}`: the idempotency table cannot be created"
        ))
    })?;
    Ok(Arc::new(store))
}

/// The in-memory store, for a site that says so in writing (`allow_memory_idempotency`): the
/// keys do not survive a restart and are not shared with a second node. Logs one warning.
#[must_use]
pub fn memory_idempotency_store(site: &str, clock: Arc<dyn Clock>) -> Arc<dyn IdempotencyStore> {
    tracing::warn!(
        site,
        "process routes: Idempotency-Keys are kept in memory and do not survive a restart"
    );
    Arc::new(MemoryIdempotencyStore::new(clock))
}

/// The process routes of one site over `port` and `idempotency`, as the host mounts them.
pub fn process_routes(
    port: Arc<ProcessApi>,
    idempotency: Arc<dyn IdempotencyStore>,
) -> axum::Router {
    let mut ports = Ports::new();
    ports.provide::<dyn ProcessPort>(port);
    ports.provide::<dyn IdempotencyStore>(idempotency);
    // Mounted here like the other hosted groups: the host authenticates every request before
    // the router sees it, and the port authenticates again.
    let mounted = routes::mount(&[routes::process::registration()], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the process routes were not mounted"
        );
    }
    mounted.authenticated
}

/// Registers the process API: the [`ProcessPort`] and its routes on the site's router. Inert when
/// the module is off or the process engine is not hosted.
///
/// # Errors
/// A setting that does not decode; the durable Idempotency-Key store cannot start; a
/// port provided twice.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_api")?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let (port, idempotency) = if ctx.assembling() {
        let Some(host) = ctx.port::<ProcessHost>() else {
            tracing::debug!("process_api: the process engine is not hosted; no routes");
            return Ok(());
        };
        let site = SiteId::new(&site_name).map_err(crate::error)?;
        let clock = Arc::clone(ctx.clock());
        let access = LiveAccess {
            live: Arc::clone(env.live),
            site: site.clone(),
            clock: Arc::clone(&clock),
            tasks: ctx.port::<dyn TaskStore>(),
            canceller: ctx.port::<dyn CaseTaskCanceller>(),
        };
        let definitions = PgDefinitionStore::new(env.backend.pool()).with_tenant(site.as_str());
        let catalog = Arc::clone(&env.permission.catalog);
        // Never a silent fall back to memory: a durable store that cannot start stops the start.
        let switch: IdempotencySwitch = ctx.settings_or_default("idempotency")?;
        let idempotency = if switch.allow_memory_idempotency {
            memory_idempotency_store(&site_name, Arc::clone(&clock))
        } else {
            durable_idempotency_store(
                &site_name,
                env.backend,
                Arc::clone(&clock),
                env.loaded.server.request_timeout,
            )
            .await?
        };
        let port = Arc::new(ProcessApi::new(Deps {
            definitions: Arc::new(definitions),
            case_file: Arc::new(PgAfterCaseFile::new(env.backend.clone(), catalog)),
            access: Arc::new(access),
            directory: ctx.port::<dyn RunDirectory>().or_else(|| {
                let store = spot_bpm_pg::PgCaseStore::new(env.backend.pool())
                    .with_tenant(site.as_str());
                Some(Arc::new(PgRunDirectory::new(store)) as Arc<dyn RunDirectory>)
            }),
            audit: Arc::new(log_audit),
            site,
            host,
            clock,
        }));
        let mut built = built().lock().unwrap_or_else(|p| p.into_inner());
        built.insert(
            site_name,
            Built {
                port: Arc::clone(&port),
                idempotency: Arc::clone(&idempotency),
            },
        );
        (port, idempotency)
    } else {
        let built = built().lock().unwrap_or_else(|p| p.into_inner());
        let Some(made) = built.get(&site_name) else {
            return Ok(());
        };
        (Arc::clone(&made.port), Arc::clone(&made.idempotency))
    };
    ctx.provide::<dyn ProcessPort>(Arc::clone(&port) as Arc<dyn ProcessPort>)?;
    let router = process_routes(port, idempotency);
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_switch_defaults_off_and_ignores_the_hosts_other_keys() {
        assert!(!IdempotencySwitch::default().allow_memory_idempotency);
        let empty: IdempotencySwitch = toml::from_str("").unwrap();
        assert!(!empty.allow_memory_idempotency);
        let on: IdempotencySwitch = toml::from_str(
            "allow_memory_idempotency = true\nsomething_new = 1\n[retention]\ndefault = \"24h\"\n",
        )
        .unwrap();
        assert!(
            on.allow_memory_idempotency,
            "unknown keys are not an error here"
        );
    }

    #[test]
    fn the_lease_is_twice_the_request_timeout_and_at_least_two_minutes() {
        let lease = |timeout: Duration| (timeout * 2).max(MIN_IDEMPOTENCY_LEASE);
        assert_eq!(lease(Duration::from_secs(30)), Duration::from_secs(120));
        assert_eq!(lease(Duration::from_secs(90)), Duration::from_secs(180));
    }
}
