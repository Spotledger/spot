//! The plan: what a reconciliation wants changed, and the event payloads to emit.
//!
//! A plan is data. The persistence layer applies it in one transaction and publishes the
//! payloads; this crate never stores, emits or reads a clock.

use std::collections::BTreeSet;
use std::fmt;

use spot_iam_roles::{Assignment, RoleName, Source};
use time::OffsetDateTime;

use crate::facts::UserId;

/// One change to the stored assignments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A new assignment, carrying the id its [`IdSource`](crate::IdSource) returned.
    /// **SoD hook:** the separation-of-duties check at grant time (WP-IAM-08) runs over
    /// [`Plan::grants`] before the plan is applied; this crate does not call it.
    Grant(Assignment),
    /// Ends an assignment: its `valid_to` becomes `at`.
    End {
        /// The assignment.
        id: String,
        /// Whose it is.
        user: UserId,
        /// The role it gave.
        role: RoleName,
        /// The instant it ends.
        at: OffsetDateTime,
        /// Why, in a sentence.
        reason: String,
    },
    /// Replaces an assignment in place, keeping its id: the same role and user, a new scope.
    /// Atomic, so the user never has a gap in the role.
    Replace {
        /// The assignment replaced.
        id: String,
        /// What it becomes.
        with: Assignment,
    },
}

impl Change {
    /// The assignment's id.
    pub fn id(&self) -> &str {
        match self {
            Change::Grant(a) => &a.id,
            Change::End { id, .. } | Change::Replace { id, .. } => id,
        }
    }

    /// The user the change is for.
    pub fn user(&self) -> &str {
        match self {
            Change::Grant(a) => &a.user,
            Change::End { user, .. } => user,
            Change::Replace { with, .. } => &with.user,
        }
    }

    /// The role the change is about.
    pub fn role(&self) -> &RoleName {
        match self {
            Change::Grant(a) => &a.role,
            Change::End { role, .. } => role,
            Change::Replace { with, .. } => &with.role,
        }
    }
}

/// Why an assignment ended, as the `cause` of `identity_assignment_ended`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndCause {
    /// Its `valid_to` passed.
    Expired,
    /// The rule that gave it no longer holds, was disabled or deleted.
    RuleEnded,
    /// The identity provider no longer lists the user in the mapped group.
    IdpRemoved,
    /// An administrator or the emergency process ended it.
    Removed,
}

impl EndCause {
    /// The wire value of the event field.
    pub fn as_str(self) -> &'static str {
        match self {
            EndCause::Expired => "expired",
            EndCause::RuleEnded => "rule_ended",
            EndCause::IdpRemoved => "idp_removed",
            EndCause::Removed => "removed",
        }
    }
}

/// An event to emit, as plain values. The topic names are those of the identity event
/// contract; mapping to JSON is the emitter's job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventPayload {
    /// `identity_assignment_created`.
    Created(Assignment),
    /// `identity_assignment_changed`.
    Changed {
        /// The assignment before.
        before: Box<Assignment>,
        /// The assignment after.
        after: Box<Assignment>,
    },
    /// `identity_assignment_ended`, also for expiry (`cause` = expired).
    Ended {
        /// The assignment.
        assignment: String,
        /// Whose.
        user: UserId,
        /// The role.
        role: RoleName,
        /// Where it came from.
        source: Source,
        /// Why it ended.
        cause: EndCause,
    },
}

impl EventPayload {
    /// The event topic.
    pub fn topic(&self) -> &'static str {
        match self {
            EventPayload::Created(_) => "identity_assignment_created",
            EventPayload::Changed { .. } => "identity_assignment_changed",
            EventPayload::Ended { .. } => "identity_assignment_ended",
        }
    }
}

/// A rule or mapping that could not be applied to one user. Nothing was changed for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserError {
    /// The user.
    pub user: UserId,
    /// The rule or group involved.
    pub subject: String,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} for user `{}` (`{}`): {}",
            crate::error::EVAL_FAILED,
            self.user,
            self.subject,
            self.message
        )
    }
}

/// The result of one reconciliation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The changes, ordered by user, then role, then assignment id.
    pub changes: Vec<Change>,
    /// The events to emit, in the same order as the changes that cause them (expiry events
    /// follow, by end instant).
    pub events: Vec<EventPayload>,
    /// Users the plan could not evaluate. Their assignments were left alone.
    pub errors: Vec<UserError>,
    /// Assignments whose end event this plan raises. The caller stores these ids and passes
    /// them to the next [`expire`](crate::expire) call, so an end is announced exactly once.
    pub ended_marker: Vec<String>,
}

impl Plan {
    /// Whether the plan changes nothing and announces nothing.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.events.is_empty()
    }

    /// The assignments the plan grants, for the separation-of-duties check.
    pub fn grants(&self) -> impl Iterator<Item = &Assignment> {
        self.changes.iter().filter_map(|c| match c {
            Change::Grant(a) => Some(a),
            _ => None,
        })
    }

    /// One line for the audit digest.
    pub fn summary(&self) -> String {
        let (mut grants, mut ends, mut replaces) = (0, 0, 0);
        for c in &self.changes {
            match c {
                Change::Grant(_) => grants += 1,
                Change::End { .. } => ends += 1,
                Change::Replace { .. } => replaces += 1,
            }
        }
        let expired = self
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    EventPayload::Ended {
                        cause: EndCause::Expired,
                        ..
                    }
                )
            })
            .count();
        format!(
            "grant {grants}, end {ends}, replace {replaces}, expired {expired}, errors {}",
            self.errors.len()
        )
    }

    /// The assignments after the plan is applied to `current`. Pure; the persistence layer does
    /// the same in one transaction. Used to prove idempotence.
    pub fn apply(&self, current: &[Assignment]) -> Vec<Assignment> {
        let mut out = current.to_vec();
        for change in &self.changes {
            match change {
                Change::Grant(a) => out.push(a.clone()),
                Change::End { id, at, .. } => {
                    for a in out.iter_mut().filter(|a| &a.id == id) {
                        a.valid_to = Some(*at);
                    }
                }
                Change::Replace { id, with } => {
                    for a in out.iter_mut().filter(|a| &a.id == id) {
                        *a = with.clone();
                    }
                }
            }
        }
        out
    }

    /// Sorts changes, builds events and the marker. `cause` is the end cause for `End`s.
    pub(crate) fn finish(
        mut changes: Vec<Change>,
        current: &[Assignment],
        cause: EndCause,
        errors: Vec<UserError>,
    ) -> Plan {
        changes.sort_by(|a, b| (a.user(), a.role(), a.id()).cmp(&(b.user(), b.role(), b.id())));
        let mut events = Vec::with_capacity(changes.len());
        let mut marker = BTreeSet::new();
        for c in &changes {
            match c {
                Change::Grant(a) => events.push(EventPayload::Created(a.clone())),
                Change::End { id, user, role, .. } => {
                    let source = current
                        .iter()
                        .find(|a| &a.id == id)
                        .map_or(Source::Manual, |a| a.source);
                    marker.insert(id.clone());
                    events.push(EventPayload::Ended {
                        assignment: id.clone(),
                        user: user.clone(),
                        role: role.clone(),
                        source,
                        cause,
                    });
                }
                Change::Replace { id, with } => {
                    if let Some(before) = current.iter().find(|a| &a.id == id) {
                        events.push(EventPayload::Changed {
                            before: Box::new(before.clone()),
                            after: Box::new(with.clone()),
                        });
                    }
                }
            }
        }
        let mut errors = errors;
        errors.sort_by(|a, b| (&a.user, &a.subject).cmp(&(&b.user, &b.subject)));
        Plan {
            changes,
            events,
            errors,
            ended_marker: marker.into_iter().collect(),
        }
    }
}
