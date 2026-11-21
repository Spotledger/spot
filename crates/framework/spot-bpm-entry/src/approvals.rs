//! Counting approvals from a task's audit trail (WP-BPM-05.T5, WP-BPM-05.T10).
//!
//! A node may declare `sla_auto_approve`, so the clock can complete it as `system:sla` with
//! `auto = true`. That event is never a person's approval. Neither is a completion whose actor
//! is empty or only whitespace (a malformed event, or a system path that forgot to set the
//! actor), nor one whose actor is a `system:` identity. Every "enough approvals" and "distinct
//! approver" decision in this crate goes through [`ApprovalStanding`], which counts only
//! [`human_approvers`] (which applies [`is_person`] itself); no other code may read a completion's actor as
//! an approver.
//!
//! # The only test for "enough approvals"
//!
//! [`ApprovalStanding::satisfies`] (which applies [`ApprovalStanding::counted`]) is the only
//! supported way to decide that approvals suffice. A node must never take the length of the
//! approver set itself, because that set still holds people the requirement excludes (the
//! previous approver, the creator).
//!
//! ```
//! use spot_bpm_entry::{ApprovalRequirement, ApprovalStanding};
//!
//! // No events: nobody approved, so no rule is met (even `at_least(0)` needs one person).
//! let standing = ApprovalStanding::from_events(&[]);
//! let rule = ApprovalRequirement::at_least(2).distinct_from("creator");
//! assert!(!standing.satisfies(&rule));
//! ```

use std::collections::BTreeSet;

use spot_bpm_tasks::{SLA_ACTOR, TaskAction, TaskEvent, human_approvers};

/// The text a Case shows when the clock, not a person, approved.
pub const AUTO_APPROVED_TEXT: &str = "approved automatically by SLA";

/// What the next node asks of the approvals so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApprovalRequirement {
    /// How many different people must have approved (1 when a single approval is enough).
    pub people: usize,
    /// People who do not count: the previous approver for "distinct from the previous
    /// approver", the creator for "distinct from the creator".
    pub excluded: BTreeSet<String>,
}

impl ApprovalRequirement {
    /// At least `people` different approvers.
    #[must_use]
    pub fn at_least(people: usize) -> Self {
        Self {
            people,
            excluded: BTreeSet::new(),
        }
    }

    /// Adds a person who does not count (a previous approver, the creator).
    #[must_use]
    pub fn distinct_from(mut self, person: &str) -> Self {
        self.excluded.insert(person.trim().to_owned());
        self
    }
}

/// How a step's approval is shown to the Case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDisplay {
    /// Nobody has approved.
    None,
    /// These people approved (a name each).
    People(Vec<String>),
    /// Only the clock approved: shown as [`AUTO_APPROVED_TEXT`], never as a person's name.
    AutomaticBySla {
        /// The SLA policy that approved, when the event names it.
        policy: Option<String>,
    },
}

impl ApprovalDisplay {
    /// The user-visible line.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::People(names) => names.join(", "),
            Self::AutomaticBySla { .. } => AUTO_APPROVED_TEXT.to_owned(),
        }
    }
}

/// The approvals a task's trail gives, people only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalStanding {
    people: BTreeSet<String>,
    automatic_policy: Option<Option<String>>,
}

fn is_automatic(event: &TaskEvent) -> bool {
    event.auto || event.actor == SLA_ACTOR || event.action == TaskAction::AutoCompleted
}

impl ApprovalStanding {
    /// The standing from a task's events.
    #[must_use]
    pub fn from_events(events: &[TaskEvent]) -> Self {
        let automatic_policy = events
            .iter()
            .rev()
            .find(|event| is_automatic(event))
            .map(|event| event.sla_policy.clone());
        Self {
            people: human_approvers(events),
            automatic_policy,
        }
    }

    /// The people who approved. An automatic completion is never among them.
    #[must_use]
    pub fn people(&self) -> &BTreeSet<String> {
        &self.people
    }

    /// How the approval is shown: the people, or "approved automatically by SLA" when the only
    /// approval was the clock's.
    #[must_use]
    pub fn display(&self) -> ApprovalDisplay {
        if !self.people.is_empty() {
            ApprovalDisplay::People(self.people.iter().cloned().collect())
        } else if let Some(policy) = &self.automatic_policy {
            ApprovalDisplay::AutomaticBySla {
                policy: policy.clone(),
            }
        } else {
            ApprovalDisplay::None
        }
    }

    /// The approvers that count towards `requirement` (people, minus the excluded). Decide
    /// sufficiency with [`Self::satisfies`], not by taking the length of this or of
    /// [`Self::people`].
    #[must_use]
    pub fn counted(&self, requirement: &ApprovalRequirement) -> BTreeSet<String> {
        self.people
            .difference(&requirement.excluded)
            .cloned()
            .collect()
    }

    /// Whether `requirement` is met, evaluated as if no approval had happened when the only
    /// approval was automatic. This is the only supported test for "enough approvals".
    #[must_use]
    pub fn satisfies(&self, requirement: &ApprovalRequirement) -> bool {
        self.counted(requirement).len() >= requirement.people.max(1)
    }
}
