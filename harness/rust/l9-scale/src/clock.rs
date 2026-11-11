//! The clock the harness measures with. Time is an argument everywhere else: the scheduler,
//! the metrics and the budgets take nanosecond values, and only the runner reads a [`Clock`].

use std::time::Instant;

/// A monotonic nanosecond source, counted from an arbitrary origin. It is not a wall clock and
/// nothing in a report is derived from one.
pub trait Clock: Send + Sync {
    /// Nanoseconds since the clock's origin.
    fn now_ns(&self) -> u64;
}

/// The process's monotonic clock, counted from its creation.
pub struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    /// A clock whose zero is now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}
