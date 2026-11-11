//! Time is an argument, never a wall clock.
//!
//! Every timestamp in this crate is a [`Timestamp`], whole seconds since the Unix epoch. The
//! caller reads its clock and passes the value in; nothing here opens a clock, so a test can
//! simulate a fleet over any span in no real time at all.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A point in time, in whole seconds since the Unix epoch.
///
/// Whole seconds are deliberate: health thresholds are seconds and minutes, and a sub-second
/// resolution would invite floating point where integers are enough.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The instant `seconds` seconds after the Unix epoch. Negative values are before it.
    #[must_use]
    pub const fn from_secs(seconds: i64) -> Self {
        Self(seconds)
    }

    /// The instant as seconds since the Unix epoch.
    #[must_use]
    pub const fn as_secs(self) -> i64 {
        self.0
    }

    /// This instant moved forward by `seconds`, saturating at the largest representable value.
    #[must_use]
    pub fn saturating_add_secs(self, seconds: u64) -> Self {
        let Ok(seconds) = i64::try_from(seconds) else {
            return Self(i64::MAX);
        };
        Self(self.0.saturating_add(seconds))
    }

    /// This instant moved back by `seconds`, saturating at the smallest representable value.
    #[must_use]
    pub fn saturating_sub_secs(self, seconds: u64) -> Self {
        let Ok(seconds) = i64::try_from(seconds) else {
            return Self(i64::MIN);
        };
        Self(self.0.saturating_sub(seconds))
    }

    /// How many whole seconds have passed from this instant to `now`.
    ///
    /// A `now` earlier than this instant yields `0` rather than a negative age: future
    /// observations are ignored by the evaluator and a clock step backwards must not turn a
    /// breach into an infinite duration.
    #[must_use]
    pub fn age(self, now: Timestamp) -> u64 {
        let delta = now.0.saturating_sub(self.0);
        if delta <= 0 { 0 } else { delta as u64 }
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A duration in whole seconds as plain language: `45 seconds`, `2 minutes`, `3 hours`,
/// `2 days`.
///
/// The value is rounded down to a whole unit. Zero and one are singular or plural correctly;
/// there is no float and no locale dependency here, because this text is a developer-facing
/// fallback and localised templates land with the console (ADM-09).
#[must_use]
pub fn humanise_seconds(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let (value, unit) = if seconds >= DAY {
        (seconds / DAY, "day")
    } else if seconds >= HOUR {
        (seconds / HOUR, "hour")
    } else if seconds >= MINUTE {
        (seconds / MINUTE, "minute")
    } else {
        (seconds, "second")
    };
    if value == 1 {
        format!("1 {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_are_whole_seconds_and_never_negative() {
        let start = Timestamp::from_secs(100);
        assert_eq!(start.age(Timestamp::from_secs(160)), 60);
        assert_eq!(start.age(Timestamp::from_secs(99)), 0);
    }

    #[test]
    fn durations_read_like_plain_language() {
        assert_eq!(humanise_seconds(0), "0 seconds");
        assert_eq!(humanise_seconds(1), "1 second");
        assert_eq!(humanise_seconds(59), "59 seconds");
        assert_eq!(humanise_seconds(60), "1 minute");
        assert_eq!(humanise_seconds(3_599), "59 minutes");
        assert_eq!(humanise_seconds(3_600), "1 hour");
        assert_eq!(humanise_seconds(86_400 * 2), "2 days");
    }

    #[test]
    fn saturating_arithmetic_does_not_wrap() {
        assert_eq!(
            Timestamp::from_secs(i64::MAX).saturating_add_secs(1),
            Timestamp::from_secs(i64::MAX)
        );
        assert_eq!(
            Timestamp::from_secs(i64::MIN).saturating_sub_secs(1),
            Timestamp::from_secs(i64::MIN)
        );
    }
}
