//! Typed errors for `spot-bpm-effect-checks`: stable `bpm_effect_checks.*` codes.
//!
//! Callers match on [`ChecksError::code`], never on the message. No message holds a field value,
//! a secret or text a peer sent: only counts, node ids and class names (all grammar-checked).

use crate::problem::Problem;

/// Result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, ChecksError>;

/// The stable codes of this crate.
pub mod codes {
    /// The process or the receipts are larger than the checks accept.
    pub const TOO_LARGE: &str = "bpm_effect_checks.too_large";
    /// The publish gate refused a version with error-level problems.
    pub const PUBLISH_REFUSED: &str = "bpm_effect_checks.publish_refused";
}

/// Why a check, a summary, a preview or the gate refused.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChecksError {
    /// An input is beyond the bounds the checks accept (fail closed, never a partial answer).
    #[error("the {what} has {found} entries; the checks accept at most {limit}")]
    TooLarge {
        /// What was too large, a fixed token.
        what: &'static str,
        /// How many there were.
        found: usize,
        /// The bound.
        limit: usize,
    },
    /// The version has error-level problems, so it is not published.
    #[error("the process version has {} error-level problem(s) and is not published", .problems.len())]
    PublishRefused {
        /// The error-level problems, in the order `check` reports them.
        problems: Vec<Problem>,
    },
}

impl ChecksError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => codes::TOO_LARGE,
            Self::PublishRefused { .. } => codes::PUBLISH_REFUSED,
        }
    }
}
