//! Milliseconds, as an argument.
//!
//! The engine reads no clock (ADR-068 rule 4, the brief's "no wall clock inside the library"):
//! every instant is an argument, so a caller can drive it from a monotonic clock, from epoch
//! milliseconds, or from a virtual clock in a test (ADR-067 section 7's simulator).

use serde::{Deserialize, Serialize};

/// An instant, or a duration, in whole milliseconds.
///
/// A caller keeps the instants it passes non-decreasing; the engine only ever compares them by
/// difference, and a `now` earlier than a recorded instant reads as no elapsed time rather than
/// as a negative one ([`Millis::since`]).
///
/// Integer milliseconds only: no floating point is used for a time value anywhere (ADR-012).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Millis(u64);

impl Millis {
    /// Zero milliseconds.
    pub const ZERO: Self = Self(0);

    /// One day in milliseconds, for callers that build a schedule from days.
    pub const DAY: Self = Self(24 * 60 * 60 * 1000);

    /// The given number of milliseconds.
    #[must_use]
    pub const fn new(millis: u64) -> Self {
        Self(millis)
    }

    /// The number of milliseconds as a plain integer.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// The whole milliseconds from `earlier` to `self`.
    ///
    /// Zero when `earlier` is later than `self`: a clock that went backwards must not read as
    /// negative elapsed time, and must not make an escalation fire early either.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Self {
        Self(self.0.saturating_sub(earlier.0))
    }

    /// Whether at least `duration` has elapsed since `earlier`.
    #[must_use]
    pub const fn elapsed_at_least(self, earlier: Self, duration: Self) -> bool {
        self.since(earlier).0 >= duration.0
    }
}
