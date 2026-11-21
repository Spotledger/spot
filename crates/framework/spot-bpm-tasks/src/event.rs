//! The immutable audit trail: one [`TaskEvent`] per action, with actor and on-behalf-of.
//!
//! Every action a person or the SLA takes on a task appends one event, and nothing ever edits or
//! removes one. The event carries **who** acted (`actor`), **on whose behalf** they acted
//! (`on_behalf_of`, set when a delegate completes for the person who delegated), the reason when
//! the action needed one, the chosen outcome and branch, and the instant — the instant is always
//! an argument, never read from a clock.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;

use crate::task::{Decision, TaskId};

/// What happened to a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAction {
    /// The Decision node created the task.
    Created,
    /// Someone claimed it.
    Claimed,
    /// Someone released a claim.
    Released,
    /// Someone decided it.
    Completed,
    /// The SLA's final action finished it without a human decision.
    AutoCompleted,
    /// Someone delegated it to another user.
    Delegated,
    /// A supervisor reassigned it.
    Reassigned,
    /// The SLA sent a reminder.
    Reminded,
    /// The SLA escalated it.
    Escalated,
    /// The Case was cancelled and took the task with it.
    Cancelled,
}

impl TaskAction {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Claimed => "claimed",
            Self::Released => "released",
            Self::Completed => "completed",
            Self::AutoCompleted => "auto_completed",
            Self::Delegated => "delegated",
            Self::Reassigned => "reassigned",
            Self::Reminded => "reminded",
            Self::Escalated => "escalated",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One immutable line of a task's audit trail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    /// The event's sequence number within its task (1-based).
    pub seq: u64,
    /// The task the event belongs to.
    pub task: TaskId,
    /// What happened.
    pub action: TaskAction,
    /// Who acted: a user name, or the system principal for an SLA action.
    pub actor: String,
    /// On whose behalf the actor acted, when they acted for someone else (a delegate's
    /// delegator).
    pub on_behalf_of: Option<String>,
    /// Why the action was taken, when it needed a reason.
    pub reason: Option<String>,
    /// The chosen outcome, on a completion.
    pub choice: Option<Decision>,
    /// The branch condition the choice selected, on a completion.
    pub branch: Option<String>,
    /// A short, non-secret detail (a delegate's name, an escalation target).
    pub detail: Option<String>,
    /// When it happened (a parameter).
    #[serde(with = "crate::wire::instant")]
    pub at: OffsetDateTime,
    /// Whether the clock, not a person, took the action (an SLA auto-completion). Such an event
    /// is never a person's approval.
    #[serde(default)]
    pub auto: bool,
    /// The SLA policy that took the action, on an automatic event.
    #[serde(default)]
    pub sla_policy: Option<String>,
}

/// The actor recorded on an automatic SLA completion.
pub const SLA_ACTOR: &str = "system:sla";

/// Invisible characters that `char::is_whitespace` does not cover (soft hyphen, zero-width
/// space, joiners, directional marks, word joiner, byte-order mark).
const INVISIBLE: [char; 10] = [
    '\u{00AD}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{2060}', '\u{FEFF}',
    '\u{061C}', '\u{180E}',
];

/// The prefix of every identity that is not a person.
const SYSTEM_PREFIX: &str = "system:";

/// Whether `actor` names a person: not empty, not only whitespace or invisible characters, and
/// not a `system:` identity (which includes [`SLA_ACTOR`]). The check is on the trimmed text and
/// ignores ASCII case for the `system:` prefix, so ` System:SLA` is not a person either. The
/// rest of the id is compared as given: whether `Ann` and `ann` are one person is an identity
/// policy question.
#[must_use]
pub fn is_person(actor: &str) -> bool {
    let actor = actor.trim();
    // Zero-width and other invisible characters are not whitespace to `trim`, yet name nobody.
    !actor
        .chars()
        .all(|c| c.is_control() || INVISIBLE.contains(&c))
        && actor != SLA_ACTOR
        && !actor
            .get(..SYSTEM_PREFIX.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(SYSTEM_PREFIX))
}

/// The people who approved, from a task's audit trail. An automatic completion never counts, so
/// a later node that needs one or several distinct approvers cannot be satisfied by the clock.
/// Only actors that pass [`is_person`] are returned (no blank, invisible or `system:` actor), and
/// each is returned trimmed, so ` ann` and `ann` are one approver.
#[must_use]
pub fn human_approvers(events: &[TaskEvent]) -> std::collections::BTreeSet<String> {
    events
        .iter()
        .filter(|event| {
            event.action == TaskAction::Completed
                && !event.auto
                && event.actor != SLA_ACTOR
                && event.choice == Some(Decision::Approve)
                && is_person(&event.actor)
        })
        .map(|event| event.actor.trim().to_owned())
        .collect()
}

impl TaskEvent {
    /// A new event with sequence 0; the store stamps the real sequence.
    #[must_use]
    pub fn new(task: &TaskId, action: TaskAction, actor: &str, at: OffsetDateTime) -> TaskEvent {
        TaskEvent {
            seq: 0,
            task: task.clone(),
            action,
            actor: actor.to_owned(),
            on_behalf_of: None,
            reason: None,
            choice: None,
            branch: None,
            detail: None,
            at,
            auto: false,
            sla_policy: None,
        }
    }

    /// Marks the event as taken by the clock under the SLA policy `policy`.
    #[must_use]
    pub fn automatic(mut self, policy: &str) -> TaskEvent {
        self.auto = true;
        self.sla_policy = Some(policy.to_owned());
        self
    }

    /// Sets the actor's delegator.
    #[must_use]
    pub fn on_behalf_of(mut self, user: &str) -> TaskEvent {
        self.on_behalf_of = Some(user.to_owned());
        self
    }

    /// Sets the reason.
    #[must_use]
    pub fn with_reason(mut self, reason: &str) -> TaskEvent {
        self.reason = Some(reason.to_owned());
        self
    }

    /// Sets the chosen outcome and branch.
    #[must_use]
    pub fn with_choice(mut self, decision: Decision, branch: &str) -> TaskEvent {
        self.choice = Some(decision);
        self.branch = Some(branch.to_owned());
        self
    }

    /// Sets the short detail.
    #[must_use]
    pub fn with_detail(mut self, detail: &str) -> TaskEvent {
        self.detail = Some(detail.to_owned());
        self
    }
}
