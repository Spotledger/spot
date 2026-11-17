//! Reconnect backoff: exponential from 1 s to 5 min with jitter (half to full of the
//! step), so a fleet that lost its gateway does not reconnect in lockstep. The jitter
//! source is an argument (per mille, 0..=1000): time and randomness stay injectable.

use std::time::Duration;

/// Smallest delay.
pub const MIN_SECS: u64 = 1;
/// Largest delay.
pub const MAX_SECS: u64 = 300;

/// The backoff state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// A fresh backoff (no failures).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a success: the next failure starts from the minimum again.
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// The consecutive failures so far.
    #[must_use]
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Records a failure and returns how long to wait. The un-jittered step is
    /// `min(2^failures, 300)` seconds (1, 2, 4 ...); the wait is between `max(1 s, step/2)` and `step`.
    pub fn next_delay(&mut self, jitter_per_mille: u64) -> Duration {
        let step_secs = MIN_SECS
            .checked_shl(self.failures.min(20))
            .unwrap_or(MAX_SECS)
            .clamp(MIN_SECS, MAX_SECS);
        self.failures = self.failures.saturating_add(1);
        let step_ms = step_secs * 1000;
        let low = (step_ms / 2).max(1000).min(step_ms);
        let jitter = jitter_per_mille.min(1000);
        Duration::from_millis(low + (step_ms - low) * jitter / 1000)
    }
}
