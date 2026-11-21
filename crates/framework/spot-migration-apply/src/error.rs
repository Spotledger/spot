//! Typed errors with stable [`codes`](crate::codes).

use crate::codes;

/// Anything the apply engine can refuse to do.
///
/// A failed verification is not an error here: it is an [`ApplyOutcome::Restored`] outcome. An
/// error is a failure that stops the run with the site **closed** (fail closed); only a verified
/// apply or a completed restore opens the site.
///
/// [`ApplyOutcome::Restored`]: crate::ApplyOutcome::Restored
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The run has no ticket, so nothing may start.
    #[error("{}: the run has no apply ticket", codes::NO_TICKET)]
    NoTicket,
    /// The run names no snapshot, or the snapshot it names was not taken.
    #[error("{}: the run names no taken snapshot", codes::NO_SNAPSHOT)]
    NoSnapshot,
    /// The ticket authorises a different plan.
    #[error(
        "{}: the ticket's plan digest {ticket} does not match the plan's {plan}",
        codes::TICKET_MISMATCH
    )]
    TicketMismatch {
        /// The digest the ticket carries.
        ticket: String,
        /// The digest the plan carries.
        plan: String,
    },
    /// An earlier run of a different plan is still unfinished.
    #[error(
        "{}: an unfinished run for plan {plan_digest} is still on this site",
        codes::IN_PROGRESS
    )]
    InProgress {
        /// The plan digest the unfinished run is for.
        plan_digest: String,
    },
    /// The plan was already applied and verified here.
    #[error(
        "{}: plan {plan_digest} was already applied and verified on this site",
        codes::ALREADY_APPLIED
    )]
    AlreadyApplied {
        /// The plan digest.
        plan_digest: String,
    },
    /// Writing or reading the apply journal failed.
    #[error("{}: {message}", codes::JOURNAL_FAILED)]
    JournalFailed {
        /// What the journal backend reported (never a secret).
        message: String,
    },
    /// The maintenance gate could not be closed or opened.
    #[error("{}: {message}", codes::GATE_FAILED)]
    GateFailed {
        /// What the gate reported.
        message: String,
    },
    /// In-flight transactions did not drain within the deadline.
    #[error(
        "{}: in-flight transactions did not drain within {waited_micros} microseconds",
        codes::DRAIN_TIMEOUT
    )]
    DrainTimeout {
        /// How long the drain waited, in microseconds.
        waited_micros: i64,
    },
    /// The side-effect hold could not be begun.
    #[error("{}: {message}", codes::HOLD_FAILED)]
    HoldFailed {
        /// What the hold backend reported.
        message: String,
    },
    /// A step failed.
    #[error("{}: step {index} ({id}) failed: {reason}", codes::STEP_FAILED)]
    StepFailed {
        /// The step's index in the plan.
        index: usize,
        /// The step's id.
        id: String,
        /// What the runner reported.
        reason: String,
    },
    /// A step exceeded its timeout.
    #[error(
        "{}: step {index} ({id}) exceeded {timeout_micros} microseconds",
        codes::STEP_TIMEOUT
    )]
    StepTimeout {
        /// The step's index in the plan.
        index: usize,
        /// The step's id.
        id: String,
        /// The timeout, in microseconds.
        timeout_micros: i64,
    },
    /// Compensating or restoring a step failed.
    #[error("{}: {message}", codes::RESTORE_FAILED)]
    RestoreFailed {
        /// What the runner or the port reported.
        message: String,
    },
    /// Restoring from the snapshot failed.
    #[error("{}: {message}", codes::SNAPSHOT_FAILED)]
    SnapshotFailed {
        /// What the snapshot port reported.
        message: String,
    },
    /// Releasing the held effects after verification failed.
    #[error("{}: {message}", codes::RELEASE_FAILED)]
    ReleaseFailed {
        /// What the hold backend reported.
        message: String,
    },
    /// An argument is not in its documented form.
    #[error("{}: invalid {what}: {reason}", codes::INVALID_INPUT)]
    InvalidInput {
        /// What was invalid.
        what: String,
        /// Why.
        reason: String,
    },
    /// The run was killed, simulating a crash; nothing was cleaned up.
    #[error("{}: the run was killed at {at}", codes::KILLED)]
    Killed {
        /// Where the kill happened, for the test log.
        at: String,
    },
    /// The journal says the new version is open but not verified.
    #[error(
        "{}: the journal for plan {plan_digest} opens an unverified new version",
        codes::OPEN_UNVERIFIED
    )]
    OpenUnverified {
        /// The plan digest.
        plan_digest: String,
    },
}

impl Error {
    /// The stable catalogue code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoTicket => codes::NO_TICKET,
            Self::NoSnapshot => codes::NO_SNAPSHOT,
            Self::TicketMismatch { .. } => codes::TICKET_MISMATCH,
            Self::InProgress { .. } => codes::IN_PROGRESS,
            Self::AlreadyApplied { .. } => codes::ALREADY_APPLIED,
            Self::JournalFailed { .. } => codes::JOURNAL_FAILED,
            Self::GateFailed { .. } => codes::GATE_FAILED,
            Self::DrainTimeout { .. } => codes::DRAIN_TIMEOUT,
            Self::HoldFailed { .. } => codes::HOLD_FAILED,
            Self::StepFailed { .. } => codes::STEP_FAILED,
            Self::StepTimeout { .. } => codes::STEP_TIMEOUT,
            Self::RestoreFailed { .. } => codes::RESTORE_FAILED,
            Self::SnapshotFailed { .. } => codes::SNAPSHOT_FAILED,
            Self::ReleaseFailed { .. } => codes::RELEASE_FAILED,
            Self::InvalidInput { .. } => codes::INVALID_INPUT,
            Self::Killed { .. } => codes::KILLED,
            Self::OpenUnverified { .. } => codes::OPEN_UNVERIFIED,
        }
    }

    /// Whether this is the sentinel a fault-injection kill returns.
    ///
    /// A kill is not handled: it stands for the process dying, so the engine returns it straight
    /// out without opening the gate, releasing the hold or restoring anything.
    #[must_use]
    pub fn is_killed(&self) -> bool {
        matches!(self, Self::Killed { .. })
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
