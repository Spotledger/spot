//! The task inbox seam of native API 0.11.0 (`task_list`, `task_get`, `task_claim`,
//! `task_release`, `task_complete`, `task_delegate`, `task_reassign`, WP-BPM-05.T7).
//!
//! `spot-http` does not depend on `spot-bpm-tasks`. It defines [`TaskPort`], the one seam the
//! route group needs, and the adapter over `spot-bpm-tasks::TaskService` is WP-BPM-05.T8.
//! [`MemoryTaskPort`] is the in-memory fake the tests use.
//!
//! # Who decides what
//!
//! The port decides **everything about rights**: who the caller is (from `ctx.credential` on
//! `ctx.site`), whether the caller may see, act on or supervise a task, segregation of duties,
//! the version check and the audit of every refusal. The routes only shape: they validate the
//! request, hand the port the authenticated context and render the answer. The routes never pass
//! a user, a role or a tenant: there is no way to ask the port to act as somebody else.
//!
//! A task the caller may neither act on nor supervise is [`TaskPortError::NotFound`], the one
//! answer an unknown task gets, so the port is never an existence oracle.
//!
//! # Idempotency of `task_complete`
//!
//! The registered route groups run outside the router's idempotency middleware, so the port owns
//! the replay of `task_complete`: the route checks that an `Idempotency-Key` is present and
//! well formed and puts it in `ctx.idempotency_key`; the port returns the first answer for a
//! repeated key (`replayed: true`) and [`TaskPortError::IdempotencyConflict`] for the same key
//! with another request.

use serde_json::{Map, Value as Json};
use time::OffsetDateTime;

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`TaskPort`].
pub const KEY: PortKey = PortKey::of::<dyn TaskPort>();

/// The contract's `TaskState`: `open` and `claimed` are active, the rest final.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    /// Waiting for somebody to claim or complete it.
    Open,
    /// Claimed by one user.
    Claimed,
    /// Decided.
    Completed,
    /// Cancelled with its run.
    Cancelled,
    /// Past its expiry.
    Expired,
}

impl TaskState {
    /// The contract's token.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Open => "open",
            TaskState::Claimed => "claimed",
            TaskState::Completed => "completed",
            TaskState::Cancelled => "cancelled",
            TaskState::Expired => "expired",
        }
    }

    /// Whether the task can still be acted on.
    pub fn is_active(self) -> bool {
        matches!(self, TaskState::Open | TaskState::Claimed)
    }
}

/// The contract's `TaskItem`.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskView {
    /// The task id (`^[a-z][a-z0-9_]{0,63}$`).
    pub id: String,
    /// The run id (16 to 64 URL-safe characters).
    pub run: String,
    /// The process's storage name.
    pub process: String,
    /// The step's node name, 1 to 128 characters.
    pub node: String,
    /// The task's label, 1 to 280 characters.
    pub label: String,
    /// The state.
    pub state: TaskState,
    /// The SLA due time, a UTC RFC 3339 instant ending in `Z`.
    pub due_at: Option<String>,
    /// The users who may act on it (at most 100).
    pub assignees: Vec<String>,
    /// The user who holds the claim.
    pub claimed_by: Option<String>,
    /// The document the step guards: the DocType and the record id.
    pub document: Option<(String, String)>,
    /// The version, at least 1; it is the `ETag`.
    pub version: u64,
}

/// One outcome the caller may choose (`TaskOutcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskOutcomeOut {
    /// The token to send in `task_complete`.
    pub outcome: String,
    /// The label, 1 to 140 characters.
    pub label: String,
}

/// One form field the step collects (`TaskField`): the name and the type only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskFieldOut {
    /// The field name (an identifier).
    pub name: String,
    /// The contract's `FieldType` token.
    pub field_type: String,
}

/// The kind of one task event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskEventKind {
    /// The task was created.
    Created,
    /// It was claimed.
    Claimed,
    /// The claim was given back.
    Released,
    /// It was decided.
    Completed,
    /// It was decided by the system.
    AutoCompleted,
    /// It was delegated.
    Delegated,
    /// It was reassigned.
    Reassigned,
    /// A reminder went out.
    Reminded,
    /// It was escalated.
    Escalated,
    /// It was cancelled.
    Cancelled,
}

impl TaskEventKind {
    /// The contract's token.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskEventKind::Created => "created",
            TaskEventKind::Claimed => "claimed",
            TaskEventKind::Released => "released",
            TaskEventKind::Completed => "completed",
            TaskEventKind::AutoCompleted => "auto_completed",
            TaskEventKind::Delegated => "delegated",
            TaskEventKind::Reassigned => "reassigned",
            TaskEventKind::Reminded => "reminded",
            TaskEventKind::Escalated => "escalated",
            TaskEventKind::Cancelled => "cancelled",
        }
    }
}

/// One event of a task (`TaskEvent`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskEventOut {
    /// When, a UTC RFC 3339 instant ending in `Z`.
    pub at: String,
    /// What happened.
    pub kind: TaskEventKind,
    /// A user id or `system:sla`.
    pub actor: String,
    /// The user the actor acted for.
    pub on_behalf_of: Option<String>,
    /// The user it went to.
    pub to: Option<String>,
    /// The outcome chosen.
    pub outcome: Option<String>,
}

/// The answer of `task_get` (`TaskDetail`).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskDetailOut {
    /// The task.
    pub task: TaskView,
    /// The outcomes the caller may choose (empty for a caller who may not act).
    pub outcomes: Vec<TaskOutcomeOut>,
    /// The form fields the step collects.
    pub fields: Vec<TaskFieldOut>,
    /// The task's events, oldest first.
    pub events: Vec<TaskEventOut>,
}

/// The list views of `task_list`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskListView {
    /// Open tasks the caller may act on.
    Mine,
    /// Tasks the caller supervises.
    Team,
    /// `Mine` past the SLA due time.
    Overdue,
}

/// The validated query of `task_list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskListIn {
    /// The view.
    pub view: TaskListView,
    /// Narrow to one process.
    pub process: Option<String>,
    /// The `next_cursor` of the previous page.
    pub cursor: Option<String>,
    /// The page size, 1 to 100.
    pub limit: usize,
}

/// One page of `task_list`.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskPageOut {
    /// The tasks the caller may see, already filtered by rights.
    pub tasks: Vec<TaskView>,
    /// The cursor of the next page; `None` on the last.
    pub next_cursor: Option<String>,
}

/// The validated body of `task_complete`.
#[derive(Clone, Debug, PartialEq)]
pub struct CompleteIn {
    /// The outcome token.
    pub outcome: String,
    /// A comment of at most 2000 characters.
    pub comment: Option<String>,
    /// The values of the form fields, bounded by the route.
    pub values: Option<Map<String, Json>>,
}

/// The validated body of `task_delegate`.
#[derive(Clone, Debug, PartialEq)]
pub struct DelegateIn {
    /// The user the task goes to.
    pub to: String,
    /// When the delegation lapses.
    pub until: Option<OffsetDateTime>,
    /// Why, at most 500 characters.
    pub reason: Option<String>,
}

/// The validated body of `task_reassign`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReassignIn {
    /// The user who becomes the only assignee.
    pub to: String,
    /// Why, at most 500 characters.
    pub reason: Option<String>,
}

/// The answer of `task_complete`.
#[derive(Clone, Debug, PartialEq)]
pub struct CompleteOut {
    /// The completed task.
    pub task: TaskView,
    /// Whether this is the replay of an earlier answer for the same `Idempotency-Key`.
    pub replayed: bool,
}

/// Why an operation was refused or failed. The variants carry no text: the route answers with
/// the registry's message for the code, so nothing a port knows reaches the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskPortError {
    /// `404 task.not_found`: unknown, or the caller may neither act on nor supervise it.
    NotFound,
    /// `409 task.already_claimed`.
    AlreadyClaimed,
    /// `409 task.not_claimed_by_caller`.
    NotClaimedByCaller,
    /// `422 task.outcome_invalid`.
    OutcomeInvalid,
    /// `403 task.segregation`.
    Segregation,
    /// `422 task.delegation_invalid`.
    DelegationInvalid,
    /// `409 task.not_active`.
    NotActive,
    /// `422 task.target_invalid`: unknown, inactive or the caller, the same answer for each.
    TargetInvalid,
    /// `412 document.stale_version`; `current` is the task's version now.
    StaleVersion {
        /// The version now.
        current: u64,
    },
    /// `403 permission.denied`: the caller sees the task but may not do this to it.
    PermissionDenied,
    /// `409 api.idempotency_conflict`: the key was used for another request.
    IdempotencyConflict,
    /// `400 api.invalid_request`: the cursor is not one this inbox issued.
    CursorInvalid,
    /// `401 auth.session_expired`: the credential is missing, unknown or ended.
    Unauthenticated,
    /// `503 server.unavailable`: the same request may succeed later.
    Unavailable,
    /// `500 http.internal`, with no detail.
    Internal,
}

impl TaskPortError {
    /// The code this refusal is audited under (a registry code, never a value).
    pub fn code(self) -> &'static str {
        match self {
            TaskPortError::NotFound => "task.not_found",
            TaskPortError::AlreadyClaimed => "task.already_claimed",
            TaskPortError::NotClaimedByCaller => "task.not_claimed_by_caller",
            TaskPortError::OutcomeInvalid => "task.outcome_invalid",
            TaskPortError::Segregation => "task.segregation",
            TaskPortError::DelegationInvalid => "task.delegation_invalid",
            TaskPortError::NotActive => "task.not_active",
            TaskPortError::TargetInvalid => "task.target_invalid",
            TaskPortError::StaleVersion { .. } => "document.stale_version",
            TaskPortError::PermissionDenied => "permission.denied",
            TaskPortError::IdempotencyConflict => "api.idempotency_conflict",
            TaskPortError::CursorInvalid => "api.invalid_request",
            TaskPortError::Unauthenticated => "auth.session_expired",
            TaskPortError::Unavailable => "server.unavailable",
            TaskPortError::Internal => "http.internal",
        }
    }
}

/// The task inbox. Every method answers for the caller `ctx.credential` names on `ctx.site`;
/// a task of another user's inbox or another site is never returned or consulted.
pub trait TaskPort: Send + Sync + 'static {
    /// `task_list`: the caller's page of tasks, filtered by rights before it is paged.
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a TaskListIn,
    ) -> BoxFuture<'a, Result<TaskPageOut, TaskPortError>>;

    /// `task_get`.
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, Result<TaskDetailOut, TaskPortError>>;

    /// `task_claim`; `expected` is the `If-Match` version.
    fn claim<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
    ) -> BoxFuture<'a, Result<TaskView, TaskPortError>>;

    /// `task_release`; `expected` is the `If-Match` version.
    fn release<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
    ) -> BoxFuture<'a, Result<TaskView, TaskPortError>>;

    /// `task_complete`; `expected` is the `If-Match` version and `ctx.idempotency_key` the
    /// required `Idempotency-Key`.
    fn complete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a CompleteIn,
    ) -> BoxFuture<'a, Result<CompleteOut, TaskPortError>>;

    /// `task_delegate`; `expected` is the `If-Match` version.
    fn delegate<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a DelegateIn,
    ) -> BoxFuture<'a, Result<TaskView, TaskPortError>>;

    /// `task_reassign`; `expected` is the `If-Match` version.
    fn reassign<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a ReassignIn,
    ) -> BoxFuture<'a, Result<TaskView, TaskPortError>>;
}

#[path = "task/memory.rs"]
mod memory;

pub use memory::{AuditRecord, MemoryTaskPort, TaskSeed};
