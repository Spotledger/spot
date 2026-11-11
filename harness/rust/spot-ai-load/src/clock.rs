//! The run clock: a monotonic time since the run began, passed to admission as an argument.
//!
//! It reads `Instant` (a monotonic clock) because latency is what the harness measures; no
//! wall clock is read anywhere in this crate.

use std::time::{Duration, Instant};

/// Time since the run started.
#[derive(Clone, Copy, Debug)]
pub struct RunClock {
    start: Instant,
}

impl RunClock {
    /// Starts the clock now.
    #[must_use]
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// The monotonic time since [`RunClock::start`].
    #[must_use]
    pub fn now(&self) -> Duration {
        self.start.elapsed()
    }
}
