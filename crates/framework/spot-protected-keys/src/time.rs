//! Monotonic time and durations, as integers.
//!
//! A lease is measured with the node's **monotonic** clock (ADR-065): a restart cannot extend
//! it. This crate never reads a clock; callers pass a [`MonoTime`] in. Durations are whole
//! milliseconds, so no floating-point arithmetic is involved.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A point on a monotonic clock, in milliseconds since an arbitrary start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MonoTime(u64);

impl MonoTime {
    /// The start of the clock.
    pub const ZERO: MonoTime = MonoTime(0);

    /// From a count of milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// The count of milliseconds.
    #[must_use]
    pub const fn millis(self) -> u64 {
        self.0
    }

    /// This instant moved forward by `duration`, saturating at the top of the counter.
    #[must_use]
    pub const fn plus(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration.0))
    }

    /// Whether this instant is at or after `other`.
    #[must_use]
    pub const fn is_at_or_after(self, other: MonoTime) -> bool {
        self.0 >= other.0
    }
}

impl fmt::Display for MonoTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

/// A whole number of milliseconds. Never a float.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Duration(u64);

impl Duration {
    /// Zero.
    pub const ZERO: Duration = Duration(0);

    /// From a count of milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// From whole seconds.
    #[must_use]
    pub const fn from_seconds(seconds: u64) -> Self {
        Self(seconds.saturating_mul(1_000))
    }

    /// From whole minutes.
    #[must_use]
    pub const fn from_minutes(minutes: u64) -> Self {
        Self(minutes.saturating_mul(60_000))
    }

    /// From whole hours.
    #[must_use]
    pub const fn from_hours(hours: u64) -> Self {
        Self(hours.saturating_mul(3_600_000))
    }

    /// The count of milliseconds.
    #[must_use]
    pub const fn millis(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn arithmetic_is_integer_and_saturating() {
        let start = MonoTime::from_millis(1_000);
        let lease = start.plus(Duration::from_minutes(15));
        assert_eq!(lease.millis(), 1_000 + 15 * 60_000);
        assert!(lease.is_at_or_after(start));
        assert_eq!(
            MonoTime::from_millis(u64::MAX).plus(Duration::from_hours(1)),
            MonoTime::from_millis(u64::MAX)
        );
        assert_eq!(Duration::from_hours(24).millis(), 86_400_000);
    }
}
