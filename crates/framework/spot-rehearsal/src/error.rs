//! Typed errors with stable [`codes`](crate::codes).

use crate::codes;

/// Anything a rehearsal can refuse to do.
///
/// Every variant is a *failure that aborts the run*; a difference the plan does not explain, or
/// a failed invariant, is part of the [`RehearsalReport`](crate::RehearsalReport), not an error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The approved ticket does not bind the plan being rehearsed.
    #[error("the ticket's plan digest {ticket} does not match the plan's {plan}")]
    TicketMismatch {
        /// The digest the ticket carries.
        ticket: String,
        /// The digest the plan carries.
        plan: String,
    },
    /// An identifier, a spec or a query is not in its documented form.
    #[error("invalid {what}: {reason}")]
    InvalidInput {
        /// What was invalid (`fingerprint spec`, `ticket id`, ...).
        what: String,
        /// Why.
        reason: String,
    },
    /// Taking the snapshot or restoring it into the scratch database failed.
    #[error("taking or restoring the clone failed: {reason}")]
    SnapshotFailed {
        /// What the provider reported.
        reason: String,
    },
    /// Applying the plan to the clone failed.
    #[error("applying the plan to the clone failed: {reason}")]
    ApplyFailed {
        /// What the applier reported.
        reason: String,
    },
    /// A fingerprint query failed, or a measure it returned was not an integer.
    #[error("computing fingerprint section {section} failed: {reason}")]
    FingerprintFailed {
        /// The section's key.
        section: String,
        /// Why.
        reason: String,
    },
}

impl Error {
    /// The stable catalogue code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::TicketMismatch { .. } => codes::TICKET_MISMATCH,
            Error::InvalidInput { .. } => codes::INVALID_INPUT,
            Error::SnapshotFailed { .. } => codes::SNAPSHOT_FAILED,
            Error::ApplyFailed { .. } => codes::APPLY_FAILED,
            Error::FingerprintFailed { .. } => codes::FINGERPRINT_FAILED,
        }
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
