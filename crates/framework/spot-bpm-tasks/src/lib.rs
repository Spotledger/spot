//! `spot-bpm-tasks` — Decision nodes and the task inbox (WS8, WP-BPM-05, ADR-49, ADR-080).
//!
//! A **Decision** node asks a person to approve, reject or choose a path. This crate is the
//! handler the engine calls for that node, the durable [`Task`] it opens, and the service that
//! drives every action on it: claim, release, complete, delegate and reassign. Each action
//! appends one immutable [`TaskEvent`], so the audit trail always answers *who held it* and *who
//! decided on whose behalf*.
//!
//! # The pieces
//!
//! - [`handler`]: [`DecisionNodeHandler`](handler::DecisionNodeHandler) returns
//!   [`WaitSpec::HumanTask`](spot_bpm_engine::WaitSpec) and opens the task, or parks the Case
//!   with `bpm_tasks.no_assignee` when the assignee does not resolve.
//! - [`task`]: the [`Task`], its options, its assignee spec and its delegation.
//! - [`event`]: the immutable [`TaskEvent`] audit line.
//! - [`store`]: the [`TaskStore`] port and the in-memory reference.
//! - [`service`]: the [`TaskService`] actions, the permission-filtered inbox and the SLA tick;
//!   a completion resumes the Case through the [`TaskEngine`] exactly once.
//! - [`sla`]: per-node reminders and escalation through a [`SlaTimerPort`], with
//!   [`BusinessCalendar`] awareness.
//! - [`access`]: who may act on or supervise a task, from the assignee set and
//!   `spot-permissions`.
//! - [`segregation`]: the creator-distinctness and `spot-sod` completion gates.
//! - [`notify`]: the [`Notifier`] port; a notification carries ids and titles only.
//! - [`text`]: the run-label pair (ADR-080); user-visible text never names a run with the
//!   internal word.
//!
//! # Purity
//!
//! No clock is read: every instant is an argument. No floats, no `unsafe`, no `unwrap`/`expect`
//! outside tests, and no secret is logged or returned (the project coding rules).

#![forbid(unsafe_code)]

pub mod access;
pub mod error;
pub mod event;
pub mod facts;
pub mod handler;
pub mod metrics;
pub mod notify;
pub mod resolve;
pub mod segregation;
pub mod service;
pub mod sla;
pub mod store;
pub mod task;
pub mod text;
pub mod wire;

pub use access::{
    AccessFuture, AssigneeAccess, DocFactsSource, InboxAccess, MapDocFacts, MapPrincipals,
    PermissionAccess, PrincipalSource, is_direct_manager, is_personal_assignee,
};
pub use error::{TasksError, TasksResult};
pub use event::{SLA_ACTOR, TaskAction, TaskEvent, human_approvers, is_person};
pub use facts::{DocumentServiceFacts, NowFn};
pub use handler::DecisionNodeHandler;
pub use metrics::{CountingMetrics, NoMetrics, SlaMetrics};
pub use notify::{
    NoNotifier, Notification, NotificationKind, Notifier, RecordingNotifier, subject,
};
pub use resolve::{
    ApproverResolver, Directory, MapDirectory, NoApprovers, Resolved, resolve_assignees,
};
pub use segregation::{DistinctFromCreator, Segregation, SegregationRefusal, SodSegregation};
pub use service::{
    EngineAdapter, RecordingEngine, SignalRecord, TaskEngine, TaskFuture, TaskService,
};
pub use sla::{
    BusinessCalendar, BusinessDaysCalendar, EngineSlaTimers, Escalation, EscalationTarget,
    FinalAction, InMemorySlaTimerPort, Recipient, Reminder, SlaPolicy, SlaStep, SlaStepKind,
    SlaTimer, SlaTimerPort, WallClockCalendar,
};
pub use store::{InMemoryTaskStore, TaskStore};
pub use task::{
    AssigneeSpec, Decision, Delegation, DelegationRequest, DelegationScope, Priority, Task,
    TaskFilter, TaskId, TaskOption, TaskSpec, TaskStatus,
};
pub use text::{RunLabel, notification_subject, user_visible_strings};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-tasks";
