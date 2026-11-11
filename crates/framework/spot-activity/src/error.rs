//! The one error type for activity failures, with stable, machine-readable `activity.*` codes.
//!
//! Messages never echo row contents or comment bodies, so nothing sensitive leaks into logs; the
//! [`ActivityError::code`] values never change in a breaking way (blueprint §4).

/// Something failed in the document activity model.
///
/// This is the one error type of this crate. A duplicate open assignment and an illegal
/// assignment transition are distinct variants, so callers can act on them without string
/// matching; for logging and API responses use [`ActivityError::code`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActivityError {
    /// `(doc, assignee)` already has an open assignment; one open assignment per pair.
    #[error("{0} already has an open assignment on {1}")]
    AssignmentDuplicate(String, String),

    /// An [`AssignmentAction`](crate::assignment::AssignmentAction) this status never accepts.
    #[error("{0} is not allowed for an assignment in status {1}")]
    AssignmentTransitionInvalid(String, String),

    /// A field path a diff or a timeline builder cannot use.
    #[error("invalid activity field path: {0}")]
    PathInvalid(String),

    /// The request carries a text that is not a valid activity record id.
    #[error("invalid record id: {0}")]
    RecordIdInvalid(String),
}

impl ActivityError {
    /// The stable machine-readable code, for logs, API responses and metrics.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::AssignmentDuplicate(_, _) => ASSIGNMENT_DUPLICATE,
            Self::AssignmentTransitionInvalid(_, _) => ASSIGNMENT_TRANSITION_INVALID,
            Self::PathInvalid(_) => PATH_INVALID,
            Self::RecordIdInvalid(_) => RECORD_ID_INVALID,
        }
    }
}

/// A field path was not usable.
pub const PATH_INVALID: &str = "activity.path_invalid";
/// The assignment id form is not valid.
pub const RECORD_ID_INVALID: &str = "activity.record_id_invalid";
/// That (document, assignee) pair already has an open assignment.
pub const ASSIGNMENT_DUPLICATE: &str = "activity.assignment_duplicate";
/// The assignment action is not one this status accepts.
pub const ASSIGNMENT_TRANSITION_INVALID: &str = "activity.assignment_transition_invalid";
