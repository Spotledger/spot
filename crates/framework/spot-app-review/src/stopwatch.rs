//! The injected clock the resource profile measures wall time with.
//!
//! The review never reads a date or a wall clock itself; a call's duration comes from a
//! [`Stopwatch`], so a test can inject a fake and the report stays reproducible.

use std::time::Instant;

/// A monotonic mark source. [`Stopwatch::mark`] never decreases between calls.
pub trait Stopwatch: Send + Sync {
    /// A monotonic mark in microseconds. The absolute value is meaningless; only the
    /// difference between two marks is used.
    fn mark(&self) -> u128;
}

/// The real stopwatch: microseconds since it was created, from a monotonic clock.
///
/// This is not a date and not the machine's wall clock; it only measures elapsed time.
pub struct SystemStopwatch {
    start: Instant,
}

impl SystemStopwatch {
    /// Starts the stopwatch now.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemStopwatch {
    fn default() -> Self {
        Self::new()
    }
}

impl Stopwatch for SystemStopwatch {
    fn mark(&self) -> u128 {
        self.start.elapsed().as_micros()
    }
}
