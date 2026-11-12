//! The injected clock. No other module reads the wall clock.

use std::sync::atomic::{AtomicI64, Ordering};

/// The current time in whole UTC seconds since the Unix epoch.
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now(&self) -> i64;
}

/// The real clock. This is the single place in the crate that reads the system time; the binary
/// wires it in, and every other component takes a [`Clock`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            })
    }
}

/// A settable clock for tests and embedders.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    /// A clock showing `seconds`.
    #[must_use]
    pub fn new(seconds: i64) -> Self {
        Self(AtomicI64::new(seconds))
    }

    /// Sets the time.
    pub fn set(&self, seconds: i64) {
        self.0.store(seconds, Ordering::SeqCst);
    }

    /// Moves the time forward.
    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, ManualClock};

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let clock = ManualClock::new(100);
        assert_eq!(clock.now(), 100);
        clock.advance(5);
        assert_eq!(clock.now(), 105);
        clock.set(7);
        assert_eq!(clock.now(), 7);
    }
}
