//! The closure error vocabulary and its stable catalogue codes (`catalogue/sync_closure.toml`).

use thiserror::Error;

/// A limit is zero or the hard limit is below the per-bundle cap.
pub const INVALID_LIMITS: &str = "sync_closure.invalid_limits";
/// A referenced record is unknown to the destination and absent at the source.
pub const TARGET_MISSING: &str = "sync_closure.target_missing";
/// A record image could not be read for its references (a dynamic link names an undeclared target).
pub const REFERENCE_UNREADABLE: &str = "sync_closure.reference_unreadable";
/// The image source failed.
pub const SOURCE_FAILED: &str = "sync_closure.source_failed";
/// The closure passed the hard limit: the `CLOSURE_TOO_LARGE` condition of ADR-067.
pub const TOO_LARGE: &str = "sync_closure.too_large";

/// Why a bundle plan could not be built.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClosureError {
    /// A limit is zero, or the hard limit is below the per-bundle record cap.
    #[error("The closure limits are not usable: {reason}.")]
    InvalidLimits {
        /// Which rule the limits break.
        reason: &'static str,
    },
    /// A reference target is neither known to the destination nor present at the source.
    #[error(
        "A referenced {entity} record ({id}) is neither known to the destination nor present at the source."
    )]
    TargetMissing {
        /// The target's entity.
        entity: String,
        /// The target's id.
        id: String,
    },
    /// A record image could not be read for its references.
    #[error(
        "A {entity} record has a link in {field} whose target cannot be determined from the metadata."
    )]
    ReferenceUnreadable {
        /// The entity whose image was read.
        entity: String,
        /// The link field.
        field: String,
    },
    /// The image source failed (an adapter error, described without any secret).
    #[error("The image source failed: {reason}.")]
    SourceFailed {
        /// A short, secret-free description.
        reason: String,
    },
    /// The closure passed the hard limit. Carries the data of the `CLOSURE_TOO_LARGE` condition.
    #[error("The closure holds {count} records, over the hard limit of {limit}.")]
    TooLarge {
        /// Records found when the limit was crossed (at least `limit + 1`; the walk stops there).
        count: usize,
        /// The hard limit in force.
        limit: usize,
    },
}

impl ClosureError {
    /// The stable dotted catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidLimits { .. } => INVALID_LIMITS,
            Self::TargetMissing { .. } => TARGET_MISSING,
            Self::ReferenceUnreadable { .. } => REFERENCE_UNREADABLE,
            Self::SourceFailed { .. } => SOURCE_FAILED,
            Self::TooLarge { .. } => TOO_LARGE,
        }
    }
}
