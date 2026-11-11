//! Typed errors with stable [`codes`](crate::codes).
//!
//! Infrastructure and precondition failures are errors: nothing was changed, or the site is
//! already closed and must stay closed. Business outcomes of the rescue itself — a blocked
//! item, a failed re-entry or a failed invariant pass — are [`RescueOutcome::Halted`] values
//! carrying the report, not errors, because the run did the right thing: it stopped and kept
//! the original data.
//!
//! [`RescueOutcome::Halted`]: crate::RescueOutcome::Halted

use crate::codes;

/// Anything the rescue tool refuses to start or cannot finish safely.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No two-person restore ticket was issued.
    #[error("{}: the rescue run has no restore ticket", codes::NO_TICKET)]
    NoTicket,
    /// The ticket authorises a different snapshot time.
    #[error(
        "{}: the ticket's snapshot time {ticket} does not match the run's {snapshot_at}",
        codes::TICKET_MISMATCH
    )]
    TicketMismatch {
        /// The snapshot time the ticket carries.
        ticket: i64,
        /// The snapshot time the run carries.
        snapshot_at: i64,
    },
    /// The ticket does not name two distinct approvers.
    #[error(
        "{}: the restore ticket names {approvers} distinct approver(s); two are required",
        codes::NOT_TWO_PERSON
    )]
    NotTwoPerson {
        /// How many distinct approvers the ticket names.
        approvers: usize,
    },
    /// The run names no snapshot.
    #[error("{}: the run names no taken snapshot", codes::NO_SNAPSHOT)]
    NoSnapshot,
    /// The snapshot could not be checked or restored.
    #[error("{}: {message}", codes::SNAPSHOT_FAILED)]
    SnapshotFailed {
        /// What the snapshot provider reported (never a secret).
        message: String,
    },
    /// The maintenance gate could not be closed or opened.
    #[error("{}: {message}", codes::GATE_FAILED)]
    GateFailed {
        /// What the gate reported.
        message: String,
    },
    /// The rescue journal could not be written or read.
    #[error("{}: {message}", codes::JOURNAL_FAILED)]
    JournalFailed {
        /// What the journal backend reported (never a secret).
        message: String,
    },
    /// At least one applied patch has no declared down-step.
    #[error(
        "{}: no down-patch plan exists; these patches are irreversible: {}",
        codes::IRREVERSIBLE,
        patches.join(", ")
    )]
    Irreversible {
        /// The patch ids that have no declared down-step.
        patches: Vec<String>,
    },
    /// An argument is not in its documented form.
    #[error("{}: invalid {what}: {reason}", codes::INVALID_INPUT)]
    InvalidInput {
        /// What was invalid.
        what: String,
        /// Why.
        reason: String,
    },
    /// The run was killed at a fault-injection boundary.
    #[error("{}: the rescue run was killed at {at}", codes::KILLED)]
    Killed {
        /// Where the kill happened, for the test log.
        at: String,
    },
    /// A down-patch step failed.
    #[error(
        "{}: down-patch {index} ({id}) failed: {reason}",
        codes::DOWN_STEP_FAILED
    )]
    DownStepFailed {
        /// The step's index in the down plan.
        index: usize,
        /// The down-patch id.
        id: String,
        /// What the runner reported.
        reason: String,
    },
    /// The journal belongs to a different rescue identity or snapshot.
    #[error("{}: the journal is for {found}, not {expected}", codes::RUN_MISMATCH)]
    RunMismatch {
        /// The identity the journal carries.
        found: String,
        /// The identity the request carries.
        expected: String,
    },
    /// A re-entered item failed in the old version's engine.
    #[error("{}: item {key} failed: {reason}", codes::REENTRY_FAILED)]
    ReentryFailed {
        /// The item key.
        key: String,
        /// What the old engine reported.
        reason: String,
    },
    /// The final invariant pass failed.
    #[error(
        "{}: {violations} invariant violation(s): {}",
        codes::INVARIANTS_FAILED,
        details.join("; ")
    )]
    InvariantsFailed {
        /// How many violations were found.
        violations: usize,
        /// The secret-free details of each violation.
        details: Vec<String>,
    },
}

impl Error {
    /// The stable catalogue code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoTicket => codes::NO_TICKET,
            Self::TicketMismatch { .. } => codes::TICKET_MISMATCH,
            Self::NotTwoPerson { .. } => codes::NOT_TWO_PERSON,
            Self::NoSnapshot => codes::NO_SNAPSHOT,
            Self::SnapshotFailed { .. } => codes::SNAPSHOT_FAILED,
            Self::GateFailed { .. } => codes::GATE_FAILED,
            Self::JournalFailed { .. } => codes::JOURNAL_FAILED,
            Self::Irreversible { .. } => codes::IRREVERSIBLE,
            Self::InvalidInput { .. } => codes::INVALID_INPUT,
            Self::Killed { .. } => codes::KILLED,
            Self::DownStepFailed { .. } => codes::DOWN_STEP_FAILED,
            Self::RunMismatch { .. } => codes::RUN_MISMATCH,
            Self::ReentryFailed { .. } => codes::REENTRY_FAILED,
            Self::InvariantsFailed { .. } => codes::INVARIANTS_FAILED,
        }
    }

    /// Whether this is the sentinel a fault-injection kill returns.
    ///
    /// A kill is not handled: it stands for the process dying, so the engine returns it
    /// straight out without opening the gate or cleaning anything up.
    #[must_use]
    pub fn is_killed(&self) -> bool {
        matches!(self, Self::Killed { .. })
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
