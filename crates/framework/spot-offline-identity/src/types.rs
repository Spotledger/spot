//! Identifiers, tiers, the node's clock and the tier bounds.
//!
//! Nothing here reads a clock: the node's monotonic reading and boot id are passed in as a
//! [`NodeClock`], and the hub's time is a plain integer ([`HubMs`]) that arrives on an
//! authenticated channel. Durations are whole milliseconds; there is no floating point.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::OfflineError;

/// Hub time in milliseconds since the Unix epoch, as the hub signs it. Never a node reading.
pub type HubMs = u64;

/// The platform floor for any tier bound (ADR-070 rule 6.4): one minute.
pub const FLOOR_MS: u64 = 60_000;
/// The platform ceiling for any tier bound (ADR-070 rule 6.4): seven days.
pub const CEILING_MS: u64 = 7 * 24 * 3_600_000;

const MAX_ID_BYTES: usize = 128;

macro_rules! id_type {
    ($name:ident, $kind:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = OfflineError;

            /// The same checks as [`Self::new`], so a deserialised id is as valid as a built one.
            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::new(&raw)
            }
        }

        impl $name {
            /// Validates `raw` (non-empty, at most 128 bytes, no control characters).
            ///
            /// # Errors
            /// [`OfflineError::InvalidId`] when `raw` is not usable.
            pub fn new(raw: &str) -> Result<Self, OfflineError> {
                if raw.is_empty() || raw.len() > MAX_ID_BYTES || raw.chars().any(char::is_control) {
                    return Err(OfflineError::InvalidId { kind: $kind });
                }
                Ok(Self(raw.to_owned()))
            }

            /// The text form.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(TenantId, "tenant id", "A tenant.");
id_type!(
    NodeId,
    "node id",
    "A branch or offline node (ADR-070 rule 12.13 h)."
);

/// The three risk tiers of ADR-070 rule 12.13 d. Tier 1 is the most sensitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// Payment posting, approvals, administrator actions and `reveal`.
    One,
    /// Reads of protected groups.
    Two,
    /// Ordinary work.
    Three,
}

impl Tier {
    /// All tiers, most sensitive first.
    pub const ALL: [Tier; 3] = [Tier::One, Tier::Two, Tier::Three];

    /// The position in per-tier arrays: 0, 1 or 2.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::One => 0,
            Self::Two => 1,
            Self::Three => 2,
        }
    }

    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::One => "tier1",
            Self::Two => "tier2",
            Self::Three => "tier3",
        }
    }
}

/// One reading of the node's clocks. `mono_ms` is monotonic within one boot and restarts at an
/// arbitrary value on the next; `boot_id` changes on every boot. The wall clock is not here
/// because nothing in this crate may use it for lease arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeClock {
    /// Milliseconds on the monotonic clock.
    pub mono_ms: u64,
    /// The id of the boot the reading belongs to.
    pub boot_id: u64,
}

/// A bound for each tier, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierBounds([u64; 3]);

impl TierBounds {
    /// The shipped defaults: tier 1 one hour, tier 2 twelve hours, tier 3 twenty-four hours.
    pub const DEFAULT: TierBounds = TierBounds([3_600_000, 12 * 3_600_000, 24 * 3_600_000]);

    /// Bounds clamped into the platform floor and ceiling, so a tenant setting can tighten
    /// or loosen a tier but never leave `[1 minute, 7 days]`.
    #[must_use]
    pub fn clamped(one_ms: u64, two_ms: u64, three_ms: u64) -> Self {
        Self([clamp(one_ms), clamp(two_ms), clamp(three_ms)])
    }

    /// Whether every bound lies inside the floor and ceiling.
    #[must_use]
    pub fn in_range(&self) -> bool {
        self.0.iter().all(|b| (FLOOR_MS..=CEILING_MS).contains(b))
    }

    /// The bound of `tier` in milliseconds.
    #[must_use]
    pub fn get(&self, tier: Tier) -> u64 {
        self.0[tier.index()]
    }
}

impl Default for TierBounds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `value` moved into `[FLOOR_MS, CEILING_MS]`.
#[must_use]
pub fn clamp(value: u64) -> u64 {
    value.clamp(FLOOR_MS, CEILING_MS)
}
