//! The small value types the planner is written in: node and release identifiers, the monotonic
//! clock reading, soak durations, percentages, seeds and the deterministic generator.
//!
//! Time is always an argument. [`Moment`] is a caller-supplied, monotonic millisecond reading;
//! nothing in this crate reads the wall clock. Randomness is a seeded [`SplitMix64`], so a
//! schedule or a simulated failure is reproducible from its [`Seed`] alone.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// The longest identifier this crate stores.
const MAX_ID_LEN: usize = 256;

fn check_id(kind: &'static str, value: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::InvalidId {
            kind: kind.to_owned(),
            reason: "it is empty".to_owned(),
        });
    }
    if value.len() > MAX_ID_LEN {
        return Err(Error::InvalidId {
            kind: kind.to_owned(),
            reason: "it is longer than 256 bytes".to_owned(),
        });
    }
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(Error::InvalidId {
            kind: kind.to_owned(),
            reason: "it contains whitespace or a control character".to_owned(),
        });
    }
    Ok(())
}

/// A node in a tenant network: the hub or one edge, named by the site and partition identity the
/// sync protocol uses (`<site>#<partition>`, ADR-056).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(String);

impl NodeId {
    /// A node identifier.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidId`] when the value is empty, longer than 256 bytes or contains
    /// whitespace or control characters.
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        check_id("node", &value)?;
        Ok(Self(value))
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NodeId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<NodeId> for String {
    fn from(value: NodeId) -> Self {
        value.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A release: the plan digest or version string the rollout is applying.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReleaseId(String);

impl ReleaseId {
    /// A release identifier.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidId`] when the value is empty, longer than 256 bytes or contains
    /// whitespace or control characters.
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        check_id("release", &value)?;
        Ok(Self(value))
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ReleaseId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ReleaseId> for String {
    fn from(value: ReleaseId) -> Self {
        value.0
    }
}

impl fmt::Display for ReleaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A monotonic millisecond reading supplied by the caller.
///
/// The absolute origin does not matter; only differences do. Using a monotonic source (rather
/// than the wall clock) means a clock correction can never make a soak elapse early.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Moment(u64);

impl Moment {
    /// The origin.
    pub const ZERO: Self = Self(0);

    /// A reading in milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// A reading in whole seconds.
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(1_000))
    }

    /// The reading in milliseconds.
    #[must_use]
    pub const fn millis(self) -> u64 {
        self.0
    }

    /// The reading advanced by `millis`, saturating at `u64::MAX`.
    #[must_use]
    pub const fn saturating_add(self, millis: u64) -> Self {
        Self(self.0.saturating_add(millis))
    }

    /// The whole milliseconds from `earlier` to `self`; zero if `earlier` is later.
    #[must_use]
    pub const fn elapsed_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// How long a wave must stay healthy before the next one may start.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Soak(u64);

impl Soak {
    /// No soak.
    pub const ZERO: Self = Self(0);

    /// A soak of `millis` milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// A soak of `secs` whole seconds.
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(1_000))
    }

    /// The soak in milliseconds.
    #[must_use]
    pub const fn millis(self) -> u64 {
        self.0
    }
}

/// A whole percentage, `0..=100`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Percent(u8);

impl Percent {
    /// Every node.
    pub const ALL: Self = Self(100);

    /// A percentage.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPlan`] when `value` is above 100.
    pub fn new(value: u8) -> Result<Self, Error> {
        if value > 100 {
            return Err(Error::InvalidPlan {
                reason: format!("a percentage is {value}, above 100"),
            });
        }
        Ok(Self(value))
    }

    /// The percentage.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Percent {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Percent> for u8 {
    fn from(value: Percent) -> Self {
        value.0
    }
}

/// The seed of a deterministic schedule or simulated failure.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Seed(u64);

impl Seed {
    /// A seed.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The seed value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// SplitMix64: a small, fast, deterministic generator. It is used only for reproducible order
/// and fault injection, never for anything security-sensitive.
#[derive(Clone, Debug)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// A generator starting from `seed`.
    pub(crate) const fn new(seed: Seed) -> Self {
        Self { state: seed.get() }
    }

    /// The next value.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..upper`; zero when `upper` is zero.
    pub(crate) fn below(&mut self, upper: u64) -> u64 {
        if upper == 0 {
            0
        } else {
            self.next_u64() % upper
        }
    }

    /// A Fisher-Yates shuffle.
    pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let other = self.below((index + 1) as u64) as usize;
            items.swap(index, other);
        }
    }
}

/// A seed derived from `seed` and a wave index, so one wave's order does not shift when another
/// wave's size changes.
pub(crate) fn wave_seed(seed: Seed, wave: usize) -> Seed {
    let mut generator = SplitMix64::new(Seed(
        seed.get() ^ (wave as u64).wrapping_mul(0xD1B5_4A32_D192_ED03),
    ));
    Seed(generator.next_u64())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_whitespace_and_empty() {
        assert!(NodeId::new("").is_err());
        assert!(NodeId::new("a b").is_err());
        assert!(NodeId::new("site#1").is_ok());
    }

    #[test]
    fn shuffle_is_reproducible() {
        let mut first = vec![1, 2, 3, 4, 5, 6];
        let mut second = vec![1, 2, 3, 4, 5, 6];
        SplitMix64::new(Seed::new(9)).shuffle(&mut first);
        SplitMix64::new(Seed::new(9)).shuffle(&mut second);
        assert_eq!(first, second);
    }

    #[test]
    fn wave_seeds_differ() {
        assert_ne!(wave_seed(Seed::new(4), 0), wave_seed(Seed::new(4), 1));
    }
}
