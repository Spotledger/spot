//! The assignment state machine (WP-DOC-07.T1).
//!
//! An [`Assignment`] gives a document to a user: work someone should do. The state machine is
//! [`AssignmentStatus::transition`]: one of three statuses, two actions, and every illegal move
//! reported as a typed error with the stable code `activity.assignment_transition_invalid`. The
//! engine keeps at most one *open* assignment per (document, assignee); enforcing it when a new
//! one is built is [`Assignment::new_open`], which fails with `activity.assignment_duplicate`.
//!
//! Nothing here persists anything; nothing reads a clock: `at` and `due` are parameters.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::{OffsetDateTime, RecordId};

use crate::error::ActivityError;

/// The status of an assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentStatus {
    /// Work to do (the only status a new assignment starts in).
    Open,
    /// The assignee finished it.
    Done,
    /// The caller withdrew it (or cancelled the document).
    Cancelled,
}

impl AssignmentStatus {
    /// The state machine: what `action` does from this status.
    ///
    /// | from | `Complete` | `Cancel` |
    /// | --- | --- | --- |
    /// | `Open` | ok → `Done` | ok → `Cancelled` |
    /// | `Done` | error | error |
    /// | `Cancelled` | error | error |
    ///
    /// A finished or withdrawn assignment is final: reopen starts a *new* assignment, which
    /// keeps the history honest (every transition is one line in the timeline).
    pub fn transition(self, action: AssignmentAction) -> Result<AssignmentStatus, ActivityError> {
        match (self, action) {
            (AssignmentStatus::Open, AssignmentAction::Complete) => Ok(AssignmentStatus::Done),
            (AssignmentStatus::Open, AssignmentAction::Cancel) => Ok(AssignmentStatus::Cancelled),
            (from, action) => Err(ActivityError::AssignmentTransitionInvalid(
                format!("{action:?}"),
                format!("{from:?}"),
            )),
        }
    }

    /// True while the assignment is still work to do.
    #[must_use]
    pub fn is_open(self) -> bool {
        self == AssignmentStatus::Open
    }
}

impl std::fmt::Display for AssignmentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AssignmentStatus::Open => "open",
            AssignmentStatus::Done => "done",
            AssignmentStatus::Cancelled => "cancelled",
        })
    }
}

/// What someone does to an assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentAction {
    /// The assignee finished the work.
    Complete,
    /// The caller withdrew it.
    Cancel,
}

/// One assignment of a document to a user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    /// The assignment's own record id.
    #[serde(with = "crate::wire::record_id")]
    pub id: RecordId,
    /// The document the work sits on.
    #[serde(with = "crate::wire::record_id")]
    pub doc: RecordId,
    /// Who should do the work, as the user's storage name.
    pub assignee: String,
    /// Who gave it, as the user's (or component's) name.
    pub assigned_by: String,
    /// When it was given (a parameter).
    #[serde(with = "crate::wire::rfc3339")]
    pub at: OffsetDateTime,
    /// The state machine's current state.
    pub status: AssignmentStatus,
    /// When the work is due, when one was set.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::wire::opt_rfc3339"
    )]
    pub due: Option<OffsetDateTime>,
}

impl Assignment {
    /// A new open assignment; fails with `activity.assignment_duplicate` when `open` already
    /// holds an open assignment for the same (document, assignee) pair — at most one open
    /// assignment per pair.
    ///
    /// # Errors
    ///
    /// [`ActivityError::AssignmentDuplicate`] when the pair already has an open assignment.
    pub fn new_open(
        id: RecordId,
        doc: RecordId,
        assignee: &str,
        assigned_by: &str,
        at: OffsetDateTime,
        due: Option<OffsetDateTime>,
        open: &[Assignment],
    ) -> Result<Assignment, ActivityError> {
        let duplicate = open
            .iter()
            .any(|a| a.doc == doc && a.assignee == assignee && a.status == AssignmentStatus::Open);
        if duplicate {
            return Err(ActivityError::AssignmentDuplicate(
                assignee.to_owned(),
                doc.to_string(),
            ));
        }
        Ok(Assignment {
            id,
            doc,
            assignee: assignee.to_owned(),
            assigned_by: assigned_by.to_owned(),
            at,
            status: AssignmentStatus::Open,
            due,
        })
    }

    /// The status after `action`: the state machine, applied.
    ///
    /// # Errors
    ///
    /// [`ActivityError::AssignmentTransitionInvalid`] when the action is not one this status
    /// accepts.
    pub fn apply(&mut self, action: AssignmentAction) -> Result<AssignmentStatus, ActivityError> {
        self.status = self.status.transition(action)?;
        Ok(self.status)
    }
}
