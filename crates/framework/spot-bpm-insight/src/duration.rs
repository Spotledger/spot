//! Exact integer durations in whole milliseconds; no float ever (ADR-12).

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;

/// A duration in whole milliseconds, the crate's one time unit.
///
/// Every duration is an exact integer: the difference of two argument-supplied instants, or a
/// policy threshold. Nothing here rounds to a float, so a percentile, a total and a threshold
/// compare exactly on every host.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Millis(i64);

impl Millis {
    /// Zero milliseconds.
    pub const ZERO: Self = Self(0);

    /// Builds a duration from whole milliseconds.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    /// Builds a duration from whole seconds.
    #[must_use]
    pub const fn from_seconds(seconds: i64) -> Self {
        Self(seconds.saturating_mul(1000))
    }

    /// The duration as whole milliseconds.
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// The duration as whole seconds, truncated toward zero.
    #[must_use]
    pub const fn as_seconds(self) -> i64 {
        self.0 / 1000
    }

    /// The exact duration between two instants (`to - from`), in whole milliseconds.
    ///
    /// The difference of two `OffsetDateTime`s is a `time::Duration`; its whole-millisecond
    /// count is an `i128` and is clamped to `i64` only if it cannot be represented.
    #[must_use]
    pub fn between(from: OffsetDateTime, to: OffsetDateTime) -> Self {
        let delta = (to - from).whole_milliseconds();
        let clamped = i64::try_from(delta).unwrap_or(if delta.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        });
        Self(clamped)
    }

    /// Whether the duration is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// The sum, saturating at the representable bounds.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// The difference, saturating at the representable bounds.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl core::fmt::Display for Millis {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ms", self.0)
    }
}

impl core::ops::Add for Millis {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        self.saturating_add(other)
    }
}

impl core::ops::AddAssign for Millis {
    fn add_assign(&mut self, other: Self) {
        *self = self.saturating_add(other);
    }
}

impl core::ops::Sub for Millis {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        self.saturating_sub(other)
    }
}

impl core::iter::Sum for Millis {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Self::saturating_add)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn between_is_exact_to_the_millisecond() {
        let from = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let to = OffsetDateTime::from_unix_timestamp(1_700_000_002).unwrap();
        assert_eq!(Millis::between(from, to), Millis::from_seconds(2));
        assert_eq!(Millis::between(to, from), Millis::from_millis(-2000));
    }

    #[test]
    fn seconds_and_millis_round_trip() {
        assert_eq!(Millis::from_seconds(3).as_millis(), 3000);
        assert_eq!(Millis::from_millis(3500).as_seconds(), 3);
        assert_eq!(Millis::ZERO.to_string(), "0 ms");
    }
}
