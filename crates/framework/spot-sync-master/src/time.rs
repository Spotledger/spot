//! The instant type: a caller-supplied UTC millisecond, never read from a clock.
//!
//! ADR-062's design rules and the brief's *must not* both forbid a second clock: this crate never
//! reads the wall clock. Every time is an argument, and [`Instant`] stores it as integer
//! milliseconds since the Unix epoch, so the value is float-free (ADR-12) and stable. Compare and
//! order `Instant`s; differences are [`Duration`]s.

use std::fmt;
use std::time::Duration;

/// A point in time, as integer milliseconds since the Unix epoch, UTC.
///
/// The value is exactly what a caller supplied; nothing is rounded and no float is ever produced.
/// This crate never reads a clock, so two calls with the same argument always agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(i64);

impl Instant {
    /// The instant `millis` milliseconds after the Unix epoch.
    pub const fn from_unix_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// The milliseconds since the Unix epoch.
    pub const fn unix_millis(self) -> i64 {
        self.0
    }

    /// This instant moved forward by `delta`, or `None` on overflow.
    pub fn checked_add(self, delta: Duration) -> Option<Self> {
        let millis = i64::try_from(delta.as_millis()).ok()?;
        self.0.checked_add(millis).map(Self)
    }

    /// How long after `earlier` this instant is, saturating at zero when it is not after it.
    pub fn duration_since(self, earlier: Self) -> Duration {
        let millis = u64::try_from(self.0.saturating_sub(earlier.0)).unwrap_or(0);
        Duration::from_millis(millis)
    }

    /// Whether this instant is at or after `other`.
    pub const fn is_at_or_after(self, other: Self) -> bool {
        self.0 >= other.0
    }
}

impl fmt::Display for Instant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn arithmetic_is_float_free_and_saturating() {
        let start = Instant::from_unix_millis(1_000);
        let later = start.checked_add(Duration::from_millis(250)).unwrap();
        assert_eq!(later.unix_millis(), 1_250);
        assert_eq!(later.duration_since(start), Duration::from_millis(250));
        assert_eq!(start.duration_since(later), Duration::ZERO);
        assert!(later.is_at_or_after(start));
        assert!(!start.is_at_or_after(later));
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        let max = Instant::from_unix_millis(i64::MAX);
        assert!(max.checked_add(Duration::from_millis(1)).is_none());
    }
}
