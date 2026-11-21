//! The recovery time objective (ADR-061 open question 4): the audit measures a rebuild and flags
//! a breach of the tenant's tier target.
//!
//! The target is an **argument**. Standard 4 h and premium 1 h are starting values a tenant
//! administrator tunes, not constants in code, so this module never names a tier or a duration.

use serde::{Deserialize, Serialize};

/// Whether a measured rebuild met the tenant's recovery time objective.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum RtoVerdict {
    /// The rebuild took no longer than the tier's target.
    Within,
    /// The rebuild was slower than the tier's target.
    Breached {
        /// How much slower, in milliseconds.
        over_by_millis: u64,
    },
}

impl RtoVerdict {
    /// Whether the target was met.
    #[must_use]
    pub fn is_within(self) -> bool {
        matches!(self, Self::Within)
    }

    /// How much the target was missed by, or zero when it was met.
    #[must_use]
    pub fn over_by_millis(self) -> u64 {
        match self {
            Self::Within => 0,
            Self::Breached { over_by_millis } => over_by_millis,
        }
    }
}

/// Compares a measured rebuild with the tier target it was given.
///
/// A rebuild exactly at the target is within it (the objective is "no slower than"). Both values
/// are durations in milliseconds; nothing here reads a clock.
#[must_use]
pub fn rto_check(measured_millis: u64, tier_target_millis: u64) -> RtoVerdict {
    if measured_millis <= tier_target_millis {
        RtoVerdict::Within
    } else {
        RtoVerdict::Breached {
            over_by_millis: measured_millis - tier_target_millis,
        }
    }
}
