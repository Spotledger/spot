//! Time sources. Framework code reads the time only through a [`Clock`], so tests of TTLs,
//! leases, sessions and one-time passwords never sleep.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

/// A source of wall-clock and monotonic time.
pub trait Clock: Send + Sync + 'static {
    /// The current instant, in UTC. May jump (NTP, manual changes); use it for timestamps
    /// stored or shown, never for measuring intervals.
    fn now_utc(&self) -> OffsetDateTime;

    /// Time elapsed since an arbitrary fixed origin of this clock. Never goes backwards; use it
    /// for TTLs, timeouts and intervals.
    fn monotonic(&self) -> Duration;
}

/// The real clock.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A system clock whose monotonic origin is now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_utc(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that moves only when told to, for tests.
#[derive(Debug)]
pub struct ManualClock {
    state: Mutex<(OffsetDateTime, Duration)>,
}

impl ManualClock {
    /// A manual clock at wall time `start`, with monotonic time zero.
    pub fn new(start: OffsetDateTime) -> Self {
        Self {
            state: Mutex::new((start, Duration::ZERO)),
        }
    }

    /// Moves both the wall clock and the monotonic clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        let mut s = self.lock();
        s.0 += by;
        s.1 += by;
    }

    /// Sets the wall clock only (simulates a clock change); monotonic time is unaffected.
    pub fn set_wall(&self, to: OffsetDateTime) {
        self.lock().0 = to;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (OffsetDateTime, Duration)> {
        // The guarded data is two plain values; a panic elsewhere cannot leave it inconsistent.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Clock for ManualClock {
    fn now_utc(&self) -> OffsetDateTime {
        self.lock().0
    }

    fn monotonic(&self) -> Duration {
        self.lock().1
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use time::OffsetDateTime;

    use super::{Clock, ManualClock, SystemClock};

    #[test]
    fn manual_clock_moves_only_when_told() {
        let start = OffsetDateTime::UNIX_EPOCH;
        let c = ManualClock::new(start);
        assert_eq!((c.now_utc(), c.monotonic()), (start, Duration::ZERO));
        c.advance(Duration::from_secs(90));
        assert_eq!(c.now_utc(), start + Duration::from_secs(90));
        assert_eq!(c.monotonic(), Duration::from_secs(90));
        c.set_wall(start);
        assert_eq!(
            (c.now_utc(), c.monotonic()),
            (start, Duration::from_secs(90))
        );
    }

    #[test]
    fn system_clock_is_monotonic_and_usable_as_dyn() {
        let c: Box<dyn Clock> = Box::new(SystemClock::new());
        let a = c.monotonic();
        assert!(c.monotonic() >= a);
        assert!(c.now_utc().year() >= 2026);
    }
}
