//! The core: the inbox operations, run on plain threads.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;
use spot_bpm_engine::InMemoryTimerPort;
use spot_bpm_tasks::{
    BusinessCalendar, Decision, DelegationRequest, DelegationScope, EngineSlaTimers, InboxAccess,
    SlaTimer, SlaTimerPort, Task, TaskAction, TaskEvent, TaskFilter, TaskId, TaskService,
    TaskStatus, TaskStore, TasksError,
};
use spot_bpm_tasks_pg::PgTaskStore;
use spot_core::Clock;
use spot_http::api::ports::task::{
    CompleteIn, CompleteOut, DelegateIn, ReassignIn, TaskDetailOut, TaskEventKind, TaskEventOut,
    TaskFieldOut, TaskListIn, TaskListView, TaskOutcomeOut, TaskPageOut, TaskPortError, TaskState,
    TaskView,
};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use super::AUDIT_TARGET;
use super::seams::{Callers, Stats, TaskDirectory};
use super::threads::{drive, lock};

/// How long an `Idempotency-Key` replay is kept (ADR-055).
const REPLAY_TTL: time::Duration = time::Duration::hours(24);

/// The most replays kept; the oldest goes first.
const MAX_REPLAYS: usize = 10_000;

/// What a caller is to a task: whether they may act, supervise or only see it.
#[derive(Clone, Copy)]
struct Standing {
    acts: bool,
    supervises: bool,
}

/// The operation an error is mapped for (the same refusal means different things per operation).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Read,
    Claim,
    Release,
    Complete,
    Delegate,
    Reassign,
}

pub(super) struct Replay {
    fingerprint: String,
    task: TaskView,
    at: OffsetDateTime,
}

pub(super) struct Core {
    pub(super) site: String,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) callers: Arc<dyn Callers>,
    pub(super) directory: Arc<dyn TaskDirectory>,
    pub(super) store: Arc<PgTaskStore>,
    pub(super) service: Arc<TaskService>,
    pub(super) access: Arc<dyn InboxAccess>,
    pub(super) timers: Arc<EngineSlaTimers<InMemoryTimerPort>>,
    pub(super) calendar: Arc<dyn BusinessCalendar>,
    pub(super) replays: Mutex<BTreeMap<(String, String), Replay>>,
    pub(super) schedule: Mutex<()>,
    pub(super) stats: Arc<Stats>,
}

fn store_error(error: &TasksError) -> TaskPortError {
    tracing::warn!(code = error.code(), "the task store failed");
    TaskPortError::Unavailable
}

/// Maps a task-service refusal to the port's declared code for `op`.
fn port_error(error: &TasksError, op: Op) -> TaskPortError {
    match error {
        TasksError::TaskNotFound { .. } => TaskPortError::NotFound,
        TasksError::TaskNotOpen { status, .. } => {
            if status == TaskStatus::Claimed.as_str() {
                TaskPortError::AlreadyClaimed
            } else {
                TaskPortError::NotActive
            }
        }
        TasksError::NotAssignee { .. } => {
            if op == Op::Release {
                TaskPortError::NotClaimedByCaller
            } else {
                TaskPortError::PermissionDenied
            }
        }
        TasksError::NotSupervisor { .. } | TasksError::DelegationExpired { .. } => {
            TaskPortError::PermissionDenied
        }
        TasksError::InvalidChoice { .. } => TaskPortError::OutcomeInvalid,
        TasksError::DistinctFromCreator { .. } | TasksError::SegregationBlocked { .. } => {
            TaskPortError::Segregation
        }
        TasksError::VersionConflict { current, .. } => {
            TaskPortError::StaleVersion { current: *current }
        }
        TasksError::StoreFailed { .. } | TasksError::EngineFailed { .. } => store_error(error),
        _ => {
            tracing::error!(code = error.code(), "an unexpected task service refusal");
            TaskPortError::Internal
        }
    }
}

fn stamp(at: OffsetDateTime) -> String {
    at.to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn state_of(status: TaskStatus) -> TaskState {
    match status {
        TaskStatus::Open => TaskState::Open,
        TaskStatus::Claimed => TaskState::Claimed,
        TaskStatus::Completed => TaskState::Completed,
        TaskStatus::Cancelled => TaskState::Cancelled,
        TaskStatus::Expired => TaskState::Expired,
    }
}

fn kind_of(action: TaskAction) -> TaskEventKind {
    match action {
        TaskAction::Created => TaskEventKind::Created,
        TaskAction::Claimed => TaskEventKind::Claimed,
        TaskAction::Released => TaskEventKind::Released,
        TaskAction::Completed => TaskEventKind::Completed,
        TaskAction::AutoCompleted => TaskEventKind::AutoCompleted,
        TaskAction::Delegated => TaskEventKind::Delegated,
        TaskAction::Reassigned => TaskEventKind::Reassigned,
        TaskAction::Reminded => TaskEventKind::Reminded,
        TaskAction::Escalated => TaskEventKind::Escalated,
        TaskAction::Cancelled => TaskEventKind::Cancelled,
    }
}

fn view_of(task: &Task) -> TaskView {
    TaskView {
        id: task.id.to_string(),
        run: task.case.clone(),
        process: task.process.clone(),
        node: task.node.clone(),
        label: task.title.clone(),
        state: state_of(task.status),
        due_at: task.due.map(stamp),
        assignees: task.assignees.iter().cloned().collect(),
        claimed_by: task.claimed_by.clone(),
        document: match (&task.doctype, &task.subject) {
            (Some(doctype), Some(subject)) => Some((doctype.clone(), subject.clone())),
            _ => None,
        },
        version: task.version,
    }
}

fn event_of(event: &TaskEvent) -> TaskEventOut {
    let names_target = matches!(event.action, TaskAction::Delegated | TaskAction::Reassigned);
    TaskEventOut {
        at: stamp(event.at),
        kind: kind_of(event.action),
        actor: event.actor.clone(),
        on_behalf_of: event.on_behalf_of.clone(),
        to: if names_target {
            event.detail.clone()
        } else {
            None
        },
        outcome: event.choice.as_ref().map(Decision::as_text),
    }
}

/// The form fields a step collects, names and types only, from the node's `form`.
fn fields_of_form(form: &Json) -> Vec<TaskFieldOut> {
    let Some(list) = form.get("fields").and_then(Json::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|field| {
            let name = field.get("name")?.as_str()?;
            let field_type = field.get("type").and_then(Json::as_str).unwrap_or("text");
            Some(TaskFieldOut {
                name: name.to_owned(),
                field_type: field_type.to_owned(),
            })
        })
        .take(100)
        .collect()
}

/// An outcome token as a decision: `approve`, `reject` or `choose:<path>`.
fn decision_of(token: &str) -> Option<Decision> {
    match token {
        "approve" => Some(Decision::Approve),
        "reject" => Some(Decision::Reject),
        other => other
            .strip_prefix("choose:")
            .filter(|path| !path.is_empty())
            .map(|path| Decision::Choose(path.to_owned())),
    }
}

/// The class of an outcome token, safe to log (the token itself is request input).
pub(super) fn outcome_class(token: &str) -> &'static str {
    match decision_of(token) {
        Some(Decision::Approve) => "approve",
        Some(Decision::Reject) => "reject",
        Some(Decision::Choose(_)) => "choose",
        None => "invalid",
    }
}

fn fingerprint(id: &str, body: &CompleteIn) -> String {
    let values = body
        .values
        .as_ref()
        .and_then(|values| serde_json::to_string(values).ok())
        .unwrap_or_default();
    format!(
        "{id}\u{0}{}\u{0}{}\u{0}{values}",
        body.outcome,
        body.comment.as_deref().unwrap_or("")
    )
}

fn after_of(cursor: Option<&str>) -> std::result::Result<Option<String>, TaskPortError> {
    match cursor {
        None => Ok(None),
        Some(cursor) => cursor
            .strip_prefix("c.")
            .filter(|id| TaskId::new(id).is_some())
            .map(|id| Some(id.to_owned()))
            .ok_or(TaskPortError::CursorInvalid),
    }
}

fn open_filter() -> TaskFilter {
    TaskFilter {
        status: Some(vec![TaskStatus::Open, TaskStatus::Claimed]),
        ..TaskFilter::default()
    }
}

fn absorb(found: &mut BTreeMap<String, Task>, tasks: impl IntoIterator<Item = Task>) {
    for task in tasks {
        if task.is_open() {
            found.entry(task.id.to_string()).or_insert(task);
        }
    }
}

impl Core {
    pub(super) fn now(&self) -> OffsetDateTime {
        self.clock.now_utc()
    }

    pub(super) fn audit(&self, action: &str, who: &str, id: &str, outcome: &str, result: &str) {
        let task = TaskId::new(id).map_or_else(|| "-".to_owned(), |task| task.to_string());
        tracing::info!(
            target: AUDIT_TARGET,
            site = %self.site,
            action,
            who,
            task = %task,
            outcome,
            result,
            "task inbox call"
        );
    }

    /// Runs `work`, then audits it with its result code, a refusal as well as a success.
    pub(super) fn audited<T>(
        &self,
        action: &'static str,
        who: &str,
        id: &str,
        outcome: &str,
        work: impl FnOnce() -> std::result::Result<T, TaskPortError>,
    ) -> std::result::Result<T, TaskPortError> {
        let result = work();
        let code = match &result {
            Ok(_) => "ok",
            Err(error) => error.code(),
        };
        self.audit(action, who, id, outcome, code);
        result
    }

    /// Loads the task and the caller's standing to it. A task the caller may neither act on,
    /// supervise nor read through its document is `NotFound`, the same answer as an unknown one.
    fn locate(&self, who: &str, id: &str) -> std::result::Result<(Task, Standing), TaskPortError> {
        let task_id = TaskId::new(id).ok_or(TaskPortError::NotFound)?;
        let task = self
            .service
            .load(&task_id)
            .map_err(|error| port_error(&error, Op::Read))?;
        let standing = Standing {
            acts: self.access.may_act(who, &task),
            supervises: self.access.may_supervise(who, &task),
        };
        if !standing.acts && !standing.supervises && !drive(self.access.may_see(who, &task)) {
            return Err(TaskPortError::NotFound);
        }
        Ok((task, standing))
    }

    fn ensure_current(task: &Task, expected: u64) -> std::result::Result<(), TaskPortError> {
        if task.version == expected {
            Ok(())
        } else {
            Err(TaskPortError::StaleVersion {
                current: task.version,
            })
        }
    }

    /// A target of a delegation or reassignment: an existing, active user other than the caller.
    /// Unknown, inactive, malformed and self are one answer.
    fn ensure_target(&self, to: &str, who: &str) -> std::result::Result<(), TaskPortError> {
        if to == who || !self.directory.is_active(to) {
            return Err(TaskPortError::TargetInvalid);
        }
        Ok(())
    }

    pub(super) fn list(
        &self,
        who: &str,
        query: &TaskListIn,
    ) -> std::result::Result<TaskPageOut, TaskPortError> {
        let after = after_of(query.cursor.as_deref())?;
        let now = self.now();
        let process_ok = |task: &Task| {
            query
                .process
                .as_deref()
                .is_none_or(|process| process == task.process)
        };
        let mut found: BTreeMap<String, Task> = BTreeMap::new();
        match query.view {
            TaskListView::Team => {
                let reports = self
                    .directory
                    .reports_of(who)
                    .ok_or(TaskPortError::Unavailable)?;
                if !reports.is_empty() {
                    let filter = TaskFilter {
                        process: query.process.clone(),
                        assignees: Some(reports),
                        ..open_filter()
                    };
                    absorb(
                        &mut found,
                        self.store.list(&filter).map_err(|e| store_error(&e))?,
                    );
                }
            }
            TaskListView::Mine | TaskListView::Overdue => {
                let overdue = query.view == TaskListView::Overdue;
                let filter = TaskFilter {
                    process: query.process.clone(),
                    assignees: Some(vec![who.to_owned()]),
                    due_before: overdue.then_some(now),
                    ..open_filter()
                };
                absorb(
                    &mut found,
                    self.store.list(&filter).map_err(|e| store_error(&e))?,
                );
                let roles = self
                    .directory
                    .roles_of(who)
                    .ok_or(TaskPortError::Unavailable)?;
                if !roles.is_empty() {
                    let by_role = self
                        .store
                        .list_by_role(&roles)
                        .map_err(|e| store_error(&e))?;
                    absorb(
                        &mut found,
                        by_role.into_iter().filter(|task| {
                            process_ok(task) && (!overdue || task.due.is_some_and(|due| due <= now))
                        }),
                    );
                }
            }
        }
        let limit = query.limit.clamp(1, 100);
        let mut tasks = Vec::new();
        let mut more = false;
        for (id, task) in &found {
            if after.as_deref().is_some_and(|after| id.as_str() <= after) {
                continue;
            }
            if tasks.len() == limit {
                more = true;
                break;
            }
            tasks.push(view_of(task));
        }
        let next_cursor = if more {
            tasks.last().map(|task| format!("c.{}", task.id))
        } else {
            None
        };
        Ok(TaskPageOut { tasks, next_cursor })
    }

    pub(super) fn get(
        &self,
        who: &str,
        id: &str,
    ) -> std::result::Result<TaskDetailOut, TaskPortError> {
        let (task, standing) = self.locate(who, id)?;
        let events = self
            .service
            .events(&task.id)
            .map_err(|error| port_error(&error, Op::Read))?;
        let outcomes = if standing.acts && task.is_open() {
            task.options
                .iter()
                .map(|option| TaskOutcomeOut {
                    outcome: option.decision.as_text(),
                    label: option.label.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(TaskDetailOut {
            task: view_of(&task),
            outcomes,
            fields: fields_of_form(&task.form),
            events: events.iter().map(event_of).collect(),
        })
    }

    pub(super) fn claim(
        &self,
        who: &str,
        id: &str,
        expected: u64,
    ) -> std::result::Result<TaskView, TaskPortError> {
        let (task, standing) = self.locate(who, id)?;
        if !task.is_open() {
            return Err(TaskPortError::NotActive);
        }
        if !standing.acts {
            return Err(TaskPortError::PermissionDenied);
        }
        Self::ensure_current(&task, expected)?;
        let claimed = self
            .service
            .claim(&task.id, who, self.now())
            .map_err(|error| port_error(&error, Op::Claim))?;
        Ok(view_of(&claimed))
    }

    pub(super) fn release(
        &self,
        who: &str,
        id: &str,
        expected: u64,
    ) -> std::result::Result<TaskView, TaskPortError> {
        let (task, standing) = self.locate(who, id)?;
        if !task.is_open() {
            return Err(TaskPortError::NotActive);
        }
        Self::ensure_current(&task, expected)?;
        let holds = task.claimed_by.as_deref() == Some(who);
        if !holds && !standing.supervises {
            return Err(if standing.acts {
                TaskPortError::NotClaimedByCaller
            } else {
                TaskPortError::PermissionDenied
            });
        }
        if task.claimed_by.is_none() {
            return Ok(view_of(&task));
        }
        let released = self
            .service
            .release(&task.id, who, self.now())
            .map_err(|error| port_error(&error, Op::Release))?;
        Ok(view_of(&released))
    }

    fn replay_of(
        &self,
        who: &str,
        key: &str,
        print: &str,
    ) -> std::result::Result<Option<TaskView>, TaskPortError> {
        let now = self.now();
        let mut replays = lock(&self.replays);
        replays.retain(|_, replay| now - replay.at < REPLAY_TTL);
        match replays.get(&(who.to_owned(), key.to_owned())) {
            None => Ok(None),
            Some(replay) if replay.fingerprint == print => Ok(Some(replay.task.clone())),
            Some(_) => Err(TaskPortError::IdempotencyConflict),
        }
    }

    fn remember(&self, who: &str, key: &str, print: String, task: &TaskView) {
        let mut replays = lock(&self.replays);
        if replays.len() >= MAX_REPLAYS {
            let oldest = replays
                .iter()
                .min_by_key(|(_, replay)| replay.at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                replays.remove(&oldest);
            }
        }
        replays.insert(
            (who.to_owned(), key.to_owned()),
            Replay {
                fingerprint: print,
                task: task.clone(),
                at: self.now(),
            },
        );
    }

    pub(super) fn complete(
        &self,
        who: &str,
        id: &str,
        expected: u64,
        body: &CompleteIn,
        key: Option<&str>,
    ) -> std::result::Result<CompleteOut, TaskPortError> {
        let print = fingerprint(id, body);
        if let Some(key) = key
            && let Some(first) = self.replay_of(who, key, &print)?
        {
            return Ok(CompleteOut {
                task: first,
                replayed: true,
            });
        }
        let (task, standing) = self.locate(who, id)?;
        if !task.is_open() {
            return Err(TaskPortError::NotActive);
        }
        if !standing.acts {
            return Err(TaskPortError::PermissionDenied);
        }
        Self::ensure_current(&task, expected)?;
        if task
            .claimed_by
            .as_deref()
            .is_some_and(|holder| holder != who)
        {
            return Err(TaskPortError::AlreadyClaimed);
        }
        let decision = decision_of(&body.outcome).ok_or(TaskPortError::OutcomeInvalid)?;
        let done = drive(self.service.complete(
            &task.id,
            who,
            decision,
            body.comment.as_deref(),
            self.now(),
        ))
        .map_err(|error| port_error(&error, Op::Complete))?;
        let view = view_of(&done);
        if let Some(key) = key {
            self.remember(who, key, print, &view);
        }
        Ok(CompleteOut {
            task: view,
            replayed: false,
        })
    }

    pub(super) fn delegate(
        &self,
        who: &str,
        id: &str,
        expected: u64,
        body: &DelegateIn,
    ) -> std::result::Result<TaskView, TaskPortError> {
        let (task, standing) = self.locate(who, id)?;
        if !task.is_open() {
            return Err(TaskPortError::NotActive);
        }
        if !standing.acts {
            return Err(TaskPortError::PermissionDenied);
        }
        Self::ensure_current(&task, expected)?;
        self.ensure_target(&body.to, who)?;
        let now = self.now();
        if body.until.is_some_and(|until| until <= now) {
            return Err(TaskPortError::DelegationInvalid);
        }
        let request = DelegationRequest {
            to: body.to.clone(),
            scope: DelegationScope::All,
            expires_at: body.until,
            reason: body.reason.clone(),
        };
        let delegated = drive(self.service.delegate(&task.id, who, request, now))
            .map_err(|error| port_error(&error, Op::Delegate))?;
        Ok(view_of(&delegated))
    }

    pub(super) fn reassign(
        &self,
        who: &str,
        id: &str,
        expected: u64,
        body: &ReassignIn,
    ) -> std::result::Result<TaskView, TaskPortError> {
        let (task, standing) = self.locate(who, id)?;
        if !task.is_open() {
            return Err(TaskPortError::NotActive);
        }
        if !standing.supervises {
            return Err(TaskPortError::PermissionDenied);
        }
        Self::ensure_current(&task, expected)?;
        self.ensure_target(&body.to, who)?;
        let reassigned = drive(self.service.reassign(
            &task.id,
            who,
            &body.to,
            body.reason.as_deref().unwrap_or(""),
            self.now(),
        ))
        .map_err(|error| port_error(&error, Op::Reassign))?;
        Ok(view_of(&reassigned))
    }

    /// Re-arms the SLA timer of every open task from the store: the next step is the number of
    /// reminders and escalations the task's trail already holds. Idempotent. Returns how many
    /// tasks were armed.
    pub(super) fn rearm(&self) -> std::result::Result<usize, TasksError> {
        let _serial = lock(&self.schedule);
        let mut armed = 0;
        for task in self.store.list(&open_filter())? {
            let Some(policy) = &task.sla else {
                continue;
            };
            let fired = self
                .store
                .events(&task.id)?
                .iter()
                .filter(|event| {
                    matches!(event.action, TaskAction::Reminded | TaskAction::Escalated)
                })
                .count();
            let Some(step) = policy.step(fired) else {
                continue;
            };
            self.timers.register(SlaTimer {
                task: task.id.clone(),
                case: task.case.clone(),
                node: task.node.clone(),
                step: fired,
                due: self
                    .calendar
                    .instant_after(task.created_at, step.after_seconds),
            });
            armed += 1;
        }
        Ok(armed)
    }

    /// Fires the SLA steps due at `now`.
    pub(super) fn tick(&self, now: OffsetDateTime) -> std::result::Result<Vec<TaskId>, TasksError> {
        let _serial = lock(&self.schedule);
        let result = drive(self.service.tick(now));
        self.stats.tick(result.is_err());
        result
    }
}
