//! Typed errors with stable [`codes`](crate::codes).

use crate::codes;

/// Anything a replay can refuse to do.
///
/// Every variant is a *failure that aborts the run*; a divergence a replay finds, whether a
/// declared behaviour entry explains it or not, is part of the
/// [`ReplayReport`](crate::ReplayReport), not an error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An identifier, a policy, a budget or a request is not in its documented form.
    #[error("invalid {what}: {reason}")]
    InvalidInput {
        /// What was invalid (`sample policy`, `budget`, ...).
        what: String,
        /// Why.
        reason: String,
    },
    /// Reading the event journal failed.
    #[error("reading the journal failed: {reason}")]
    JournalFailed {
        /// What the source reported.
        reason: String,
    },
    /// The document engine failed to re-execute an operation on the clone.
    #[error("re-executing {doctype} {record} failed: {reason}")]
    ExecutionFailed {
        /// The DocType of the operation.
        doctype: String,
        /// The document the operation was about.
        record: String,
        /// What the runner reported.
        reason: String,
    },
    /// The site is not held, so an unheld replay could let an effect escape.
    #[error("site {site} is not held ({state}), so replay refuses to run")]
    NotHeld {
        /// The site.
        site: String,
        /// What the hold state was (`open`, `unknown`).
        state: String,
    },
    /// An external effect of a never-held class was admitted to proceed during replay.
    #[error("external effect {key} of class {class} would leave during replay")]
    EffectEscaped {
        /// The idempotency key of the effect.
        key: String,
        /// The effect's class key.
        class: String,
    },
    /// The target a replay was asked to run against is not the clone.
    #[error("target {label} is not the clone")]
    NotClone {
        /// The label of the target a request named.
        label: String,
    },
}

impl Error {
    /// The stable catalogue code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidInput { .. } => codes::INVALID_INPUT,
            Error::JournalFailed { .. } => codes::JOURNAL_FAILED,
            Error::ExecutionFailed { .. } => codes::EXECUTION_FAILED,
            Error::NotHeld { .. } => codes::NOT_HELD,
            Error::EffectEscaped { .. } => codes::EFFECT_ESCAPED,
            Error::NotClone { .. } => codes::NOT_CLONE,
        }
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
