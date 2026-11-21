//! The planner's tuning values (ADR-066 sections 1 and 2). Settings, never hidden constants: the
//! caller owns every number here, and the ADR's proposals are only the defaults.

use crate::error::CatchUpError;
use serde::{Deserialize, Serialize};

/// A share of a whole, in basis points (1/10,000). Integer only: no float decides a mode.
///
/// The ADR writes the snapshot threshold as "a fraction (default 40%)"; 40% is 4,000 basis points,
/// which is exact, where 0.4 is not representable and money, quantities and time never touch
/// floating point here (ADR-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Share {
    basis_points: u16,
}

impl Share {
    /// The whole: 10,000 basis points (100%). The largest valid share.
    pub const WHOLE: u16 = 10_000;

    /// A share in basis points.
    ///
    /// # Errors
    /// `InvalidSettings` when the value is above [`Share::WHOLE`]: that is more than all of it.
    pub fn from_basis_points(basis_points: u16) -> Result<Self, CatchUpError> {
        let share = Self { basis_points };
        share.validate()?;
        Ok(share)
    }

    /// A whole-number percentage, for a setting written the way the ADR writes it (`40`).
    ///
    /// # Errors
    /// `InvalidSettings` when the percentage is above 100.
    pub fn from_percent(percent: u8) -> Result<Self, CatchUpError> {
        Self::from_basis_points(u16::from(percent) * 100)
    }

    /// The share in basis points.
    #[must_use]
    pub fn basis_points(&self) -> u16 {
        self.basis_points
    }

    /// Whether this share is a share of a whole at all.
    ///
    /// # Errors
    /// `InvalidSettings` when the value is above [`Share::WHOLE`].
    pub fn validate(&self) -> Result<(), CatchUpError> {
        if self.basis_points > Self::WHOLE {
            return Err(CatchUpError::InvalidSettings {
                reason: "a snapshot_fraction above 100% is not a share of a whole",
            });
        }
        Ok(())
    }
}

impl Default for Share {
    /// 4,000 basis points: the 40% starting value of ADR-066 section 2, tunable by the caller.
    fn default() -> Self {
        Self {
            basis_points: 4_000,
        }
    }
}

/// What the hub's planner is allowed to spend (ADR-066 sections 1 and 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The largest gap still replayed in position order (ADR-066 default: 10,000 in-scope events).
    /// A gap **at** the limit is incremental: the limit is the largest incremental gap.
    pub incremental_limit: u64,
    /// The share of a full scoped snapshot above which coalescing is not worth it (default 40%).
    pub snapshot_fraction: Share,
    /// The most records in one planned bundle (ADR-066 section 1 flow control: default 500).
    pub bundle_records: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            incremental_limit: 10_000,
            snapshot_fraction: Share::default(),
            bundle_records: 500,
        }
    }
}

impl Settings {
    /// Whether these settings can plan at all.
    ///
    /// # Errors
    /// `InvalidSettings` when a limit is zero or the share is above the whole.
    pub fn validate(&self) -> Result<(), CatchUpError> {
        if self.incremental_limit == 0 {
            return Err(CatchUpError::InvalidSettings {
                reason: "the incremental limit must be above zero",
            });
        }
        if self.bundle_records == 0 {
            return Err(CatchUpError::InvalidSettings {
                reason: "the bundle record cap must be above zero",
            });
        }
        self.snapshot_fraction.validate()
    }
}
