//! The human task: its id, its options, its assignee spec, its delegation, and its state.
//!
//! A [`Task`] is the durable unit a Decision node creates: one person (or a set of people) is
//! asked to approve, reject or choose a path, and every action on it appends an immutable
//! [`TaskEvent`](crate::TaskEvent). This module is pure data with stable serde: nothing here
//! reads a clock, and every transition is a method a caller drives with the instant it was
//! handed.
//!
//! A task's assignees are **resolved** when it is created (role members, named users, or an
//! approver rule's answer). The role names are kept beside the resolved users so a member added
//! to the role later can still act; resolving a role to nobody is not a guess, it parks the Case
//! with `bpm_tasks.no_assignee`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;

use crate::error::TasksError;

/// The identifier of one task: `[a-z][a-z0-9_]{0,63}`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    /// The id named `text`, or `None` when it breaks the grammar.
    #[must_use]
    pub fn new(text: &str) -> Option<TaskId> {
        if is_valid(text) {
            Some(TaskId(text.to_owned()))
        } else {
            None
        }
    }

    /// The deterministic id for the task of `(case, node)`.
    ///
    /// The natural `"{case}_{node}"` is used when it fits the grammar and the 64-character
    /// bound; otherwise a stable hash of the pair is used, so the same Decision node in the same
    /// Case always names the same task and a replay finds it instead of creating a second one.
    #[must_use]
    pub fn for_node(case: &str, node: &str) -> TaskId {
        let candidate = format!("{case}_{node}");
        if candidate.len() <= 64 && is_valid(&candidate) {
            return TaskId(candidate);
        }
        TaskId(format!("task_{:016x}", fnv1a64(candidate.as_bytes())))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for TaskId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `text` matches `[a-z][a-z0-9_]{0,63}`.
fn is_valid(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    if rest.len() > 63 {
        return false;
    }
    rest.iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// A stable 64-bit FNV-1a hash, used only to name a task deterministically when the natural id
/// would be too long. It is not a security primitive.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// How urgent a task is; higher is more urgent.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Low priority.
    Low,
    /// The ordinary priority.
    #[default]
    Normal,
    /// High priority.
    High,
    /// Urgent.
    Urgent,
}

impl Priority {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }
}

/// The lifecycle state of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting for someone to claim or act on it.
    Open,
    /// Someone claimed it, but has not decided.
    Claimed,
    /// Someone decided it; a completed task is final.
    Completed,
    /// The Case was cancelled and took the task with it.
    Cancelled,
    /// The SLA's final action expired the task without a human decision.
    Expired,
}

impl TaskStatus {
    /// Whether the task is still work to do.
    #[must_use]
    pub fn is_open(self) -> bool {
        matches!(self, Self::Open | Self::Claimed)
    }

    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Claimed => "claimed",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

/// What a person decides: approve, reject, or choose one of the node's named paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Approve.
    Approve,
    /// Reject.
    Reject,
    /// Choose the named path.
    Choose(String),
}

impl Decision {
    /// The stable text form used in events and error messages.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Approve => "approve".to_owned(),
            Self::Reject => "reject".to_owned(),
            Self::Choose(path) => format!("choose:{path}"),
        }
    }
}

/// One outcome a task offers: the label a person sees, the decision it stands for, and the
/// branch condition it selects on the node's outgoing edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOption {
    /// The label shown in the inbox.
    pub label: String,
    /// The decision this option stands for.
    pub decision: Decision,
    /// The branch condition the completion selects.
    pub branch: String,
}

/// How a Decision node names its assignee, before resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssigneeSpec {
    /// One named user.
    User(String),
    /// A role: its members are resolved to users.
    Role(String),
    /// An approver rule resolved by a rule-graph or approver-resolution function.
    Approver(String),
    /// An explicit set of users.
    Any(Vec<String>),
}

impl AssigneeSpec {
    /// Parses the node's `assignee` text: `user:<name>`, `role:<name>`, `rule:<name>` or
    /// `approver:<name>`, or a bare name, which is read as a user. Empty text is an unresolved
    /// [`AssigneeSpec::Any`] with no users.
    #[must_use]
    pub fn parse(text: &str) -> AssigneeSpec {
        let text = text.trim();
        if text.is_empty() {
            return AssigneeSpec::Any(Vec::new());
        }
        if let Some(rest) = text.strip_prefix("role:") {
            AssigneeSpec::Role(rest.trim().to_owned())
        } else if let Some(rest) = text.strip_prefix("user:") {
            AssigneeSpec::User(rest.trim().to_owned())
        } else if let Some(rest) = text.strip_prefix("rule:") {
            AssigneeSpec::Approver(rest.trim().to_owned())
        } else if let Some(rest) = text.strip_prefix("approver:") {
            AssigneeSpec::Approver(rest.trim().to_owned())
        } else {
            AssigneeSpec::User(text.to_owned())
        }
    }
}

/// What a delegation permits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationScope {
    /// The delegate may claim and complete.
    All,
    /// The delegate may claim only.
    Claim,
    /// The delegate may complete only.
    Complete,
}

impl DelegationScope {
    /// Whether this scope permits `action`.
    #[must_use]
    pub fn allows(self, action: DelegationScope) -> bool {
        matches!(self, Self::All) || self == action
    }
}

/// One delegation of a task to another user, with its scope and expiry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    /// The user the task is delegated to.
    pub to: String,
    /// The user who delegated it.
    pub by: String,
    /// What the delegate may do.
    pub scope: DelegationScope,
    /// When the delegation was made (a parameter).
    #[serde(with = "crate::wire::instant")]
    pub at: OffsetDateTime,
    /// When the delegation expires; absent means it does not.
    #[serde(default, with = "crate::wire::option_instant")]
    pub expires_at: Option<OffsetDateTime>,
}

impl Delegation {
    /// Whether the delegation is still active at `now`.
    #[must_use]
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_none_or(|expires| expires > now)
    }
}

/// The parts of a delegation request: who to, what scope, until when, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationRequest {
    /// The user the task is delegated to.
    pub to: String,
    /// What the delegate may do.
    pub scope: DelegationScope,
    /// When the delegation expires; absent means it does not.
    pub expires_at: Option<OffsetDateTime>,
    /// Why the task was delegated, for the audit trail.
    pub reason: Option<String>,
}

/// One human task created by a Decision node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// The task's id.
    pub id: TaskId,
    /// The Case the task belongs to, as text.
    pub case: String,
    /// The Decision node the task belongs to, as text.
    pub node: String,
    /// The process (the pinned DAG key) the Case runs.
    pub process: String,
    /// The question the person is asked.
    pub title: String,
    /// The DocType of the underlying document, when the node works on one.
    pub doctype: Option<String>,
    /// The underlying document's id, as text, when the node works on one.
    pub subject: Option<String>,
    /// Who created the underlying document, when it is known.
    pub created_by: Option<String>,
    /// Whether the node forbids the creator from deciding.
    pub distinct_from_creator: bool,
    /// The duty completing the task exercises, for the segregation gate.
    pub duty: Option<String>,
    /// The scope the duty is held in, when the task carries one.
    pub scope: Option<String>,
    /// How the assignee was named.
    pub assignee_spec: AssigneeSpec,
    /// The resolved assignee users.
    pub assignees: BTreeSet<String>,
    /// The role names, kept so a member added later may still act.
    pub roles: BTreeSet<String>,
    /// The outcomes the person may choose.
    pub options: Vec<TaskOption>,
    /// The form payload shown with the task (opaque JSON).
    pub form: Json,
    /// The task's priority.
    pub priority: Priority,
    /// When the task is due, when the node sets a deadline.
    #[serde(default, with = "crate::wire::option_instant")]
    pub due: Option<OffsetDateTime>,
    /// The task's status.
    pub status: TaskStatus,
    /// Who claimed it, when someone did.
    pub claimed_by: Option<String>,
    /// The active delegation, when one is in force.
    pub delegation: Option<Delegation>,
    /// The SLA policy the node attached, when it has one.
    pub sla: Option<crate::sla::SlaPolicy>,
    /// When the task was created (a parameter).
    #[serde(with = "crate::wire::instant")]
    pub created_at: OffsetDateTime,
    /// When it was completed, when it was.
    #[serde(default, with = "crate::wire::option_instant")]
    pub completed_at: Option<OffsetDateTime>,
    /// The outcome it was completed with, when it was: a duplicate submission of the same
    /// decision is idempotent, and a different one is refused.
    #[serde(default)]
    pub outcome: Option<Decision>,
    /// Whether the Case has been resumed for this task's completion. A completion commits
    /// before it signals, so a retry that finds `Completed` and this `false` resumes exactly
    /// once instead of never.
    #[serde(default)]
    pub resumed_case: bool,
    /// Whether the clock (an SLA final action), not a person, completed the task.
    #[serde(default)]
    pub auto_completed: bool,
    /// The optimistic version the store compares against.
    pub version: u64,
}

/// The parts a Decision node hands [`Task::open`] to build a task.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskSpec {
    /// The task id.
    pub id: TaskId,
    /// The Case id, as text.
    pub case: String,
    /// The node id, as text.
    pub node: String,
    /// The process (DAG key).
    pub process: String,
    /// The question.
    pub title: String,
    /// The DocType, when there is one.
    pub doctype: Option<String>,
    /// The document id, when there is one.
    pub subject: Option<String>,
    /// The document's creator, when known.
    pub created_by: Option<String>,
    /// Whether the creator may not decide.
    pub distinct_from_creator: bool,
    /// The duty completing exercises, when the node declares one.
    pub duty: Option<String>,
    /// The duty's scope, when it has one.
    pub scope: Option<String>,
    /// How the assignee was named.
    pub assignee_spec: AssigneeSpec,
    /// The resolved assignee users.
    pub assignees: BTreeSet<String>,
    /// The role names.
    pub roles: BTreeSet<String>,
    /// The outcomes.
    pub options: Vec<TaskOption>,
    /// The form payload.
    pub form: Json,
    /// The priority.
    pub priority: Priority,
    /// The due instant.
    pub due: Option<OffsetDateTime>,
    /// The SLA policy.
    pub sla: Option<crate::sla::SlaPolicy>,
    /// The creation instant.
    pub created_at: OffsetDateTime,
}

impl Task {
    /// Opens a fresh task in [`TaskStatus::Open`], version 1, with no claim and no delegation.
    #[must_use]
    pub fn open(spec: TaskSpec) -> Task {
        Task {
            id: spec.id,
            case: spec.case,
            node: spec.node,
            process: spec.process,
            title: spec.title,
            doctype: spec.doctype,
            subject: spec.subject,
            created_by: spec.created_by,
            distinct_from_creator: spec.distinct_from_creator,
            duty: spec.duty,
            scope: spec.scope,
            assignee_spec: spec.assignee_spec,
            assignees: spec.assignees,
            roles: spec.roles,
            options: spec.options,
            form: spec.form,
            priority: spec.priority,
            due: spec.due,
            status: TaskStatus::Open,
            claimed_by: None,
            delegation: None,
            sla: spec.sla,
            created_at: spec.created_at,
            completed_at: None,
            outcome: None,
            resumed_case: false,
            auto_completed: false,
            version: 1,
        }
    }

    /// Whether the task is still work to do.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.status.is_open()
    }

    /// Whether `user` is one of the resolved assignees.
    #[must_use]
    pub fn is_explicit_assignee(&self, user: &str) -> bool {
        self.assignees.contains(user)
    }

    /// The option for `decision`, when the task offers it.
    #[must_use]
    pub fn option_for(&self, decision: &Decision) -> Option<&TaskOption> {
        self.options
            .iter()
            .find(|option| &option.decision == decision)
    }

    /// Whether the task offers `decision`.
    #[must_use]
    pub fn accepts(&self, decision: &Decision) -> bool {
        self.option_for(decision).is_some()
    }

    /// The branch condition `decision` selects, when the task offers it.
    #[must_use]
    pub fn branch_for(&self, decision: &Decision) -> Option<String> {
        self.option_for(decision)
            .map(|option| option.branch.clone())
    }

    /// The active delegation at `now`, when one is in force.
    #[must_use]
    pub fn active_delegation(&self, now: OffsetDateTime) -> Option<&Delegation> {
        self.delegation.as_ref().filter(|d| d.is_active(now))
    }

    /// Refuses unless the task is open.
    ///
    /// # Errors
    /// [`TasksError::TaskNotOpen`] when the task is finished.
    pub fn ensure_open(&self) -> Result<(), TasksError> {
        if self.is_open() {
            Ok(())
        } else {
            Err(TasksError::TaskNotOpen {
                id: self.id.to_string(),
                status: self.status.as_str().to_owned(),
            })
        }
    }
}

/// A filter over stored tasks, applied by [`TaskStore::list`](crate::TaskStore::list).
///
/// Every field that is set narrows the result; an empty filter matches every task.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskFilter {
    /// Keep only tasks in one of these statuses.
    pub status: Option<Vec<TaskStatus>>,
    /// Keep only tasks of this Case.
    pub case: Option<String>,
    /// Keep only tasks of this process (the pinned DAG key).
    pub process: Option<String>,
    /// Keep only tasks with one of these assignees.
    pub assignees: Option<Vec<String>>,
    /// Keep only tasks due at or before this instant.
    pub due_before: Option<OffsetDateTime>,
}

impl TaskFilter {
    /// Whether `task` matches every set condition.
    #[must_use]
    pub fn matches(&self, task: &Task) -> bool {
        if let Some(status) = &self.status
            && !status.contains(&task.status)
        {
            return false;
        }
        if let Some(case) = &self.case
            && &task.case != case
        {
            return false;
        }
        if let Some(process) = &self.process
            && &task.process != process
        {
            return false;
        }
        if let Some(assignees) = &self.assignees
            && !assignees.iter().any(|a| task.assignees.contains(a))
        {
            return false;
        }
        if let Some(due_before) = self.due_before
            && task.due.is_none_or(|due| due > due_before)
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_follow_the_grammar() {
        assert!(TaskId::new("task_1").is_some());
        assert!(TaskId::new("1task").is_none());
        assert!(TaskId::new("Task").is_none());
        assert!(TaskId::new("").is_none());
        assert!(TaskId::new(&"a".repeat(65)).is_none());
    }

    #[test]
    fn for_node_is_deterministic_and_bounded() {
        let a = TaskId::for_node("case1", "approve");
        let b = TaskId::for_node("case1", "approve");
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "case1_approve");
        let long = TaskId::for_node(&"c".repeat(64), &"n".repeat(64));
        assert!(long.as_str().len() <= 64);
        assert_eq!(long, TaskId::for_node(&"c".repeat(64), &"n".repeat(64)));
    }

    #[test]
    fn assignee_specs_parse() {
        assert_eq!(
            AssigneeSpec::parse("role:manager"),
            AssigneeSpec::Role("manager".to_owned())
        );
        assert_eq!(
            AssigneeSpec::parse("alice"),
            AssigneeSpec::User("alice".to_owned())
        );
        assert_eq!(
            AssigneeSpec::parse("rule:credit"),
            AssigneeSpec::Approver("credit".to_owned())
        );
        assert_eq!(AssigneeSpec::parse(""), AssigneeSpec::Any(Vec::new()));
    }

    #[test]
    fn delegation_scopes_permit() {
        assert!(DelegationScope::All.allows(DelegationScope::Complete));
        assert!(DelegationScope::Claim.allows(DelegationScope::Claim));
        assert!(!DelegationScope::Claim.allows(DelegationScope::Complete));
    }
}
