//! The injectable clock: wall time for the ledger, and waiting for backoff and cool-downs.
//!
//! The router never calls `std::time` or `tokio::time` directly. It asks a [`Clock`] for the
//! time and for waits, so a test drives backoff schedules and circuit-breaker cool-downs
//! deterministically through [`ManualClock`] — no test sleeps, no flaky timing.
//!
//! Time is **epoch milliseconds as a `u64`** throughout, and a [`DayKey`](crate::DayKey) is
//! that value divided by a day, so day bucketing is integer arithmetic with no calendar
//! crate.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use spot_llm::BoxFuture;

/// The clock the router reads time from and waits on.
///
/// Two implementations ship: [`SystemClock`] for real use and [`ManualClock`] for tests and
/// deterministic embedding.
pub trait Clock: Send + Sync {
    /// The current time in milliseconds since the Unix epoch (UTC).
    fn now_millis(&self) -> u64;

    /// Waits `millis` milliseconds. [`ManualClock`] returns at once after recording the wait
    /// and advancing its own time, which is what makes backoff schedules assertable.
    fn wait(&self, millis: u64) -> BoxFuture<'_, ()>;
}

/// The real clock: `SystemTime` for reading, `tokio::time::sleep` for waiting.
///
/// `tokio`'s clock honours the paused-time mode of `#[tokio::test(start_paused = true)]`, so a
/// test that uses the system clock still waits for no real time.
#[derive(Debug, Default)]
pub struct SystemClock;

impl SystemClock {
    /// The system clock.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => u64::try_from(since.as_millis()).unwrap_or(u64::MAX),
            // A clock before the epoch is not a case the platform supports; report the epoch
            // rather than panicking inside a request path.
            Err(_) => 0,
        }
    }

    fn wait(&self, millis: u64) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(millis)))
    }
}

/// A clock the caller moves: waiting advances it, so backoff and cool-downs run instantly and
/// the schedule is inspectable.
///
/// ```no_run
/// use spot_llm_router::clock::{Clock, ManualClock};
///
/// let clock = ManualClock::at(1_700_000_000_000);
/// # async {
/// clock.wait(250).await;
/// # };
/// assert_eq!(clock.waits(), vec![250]);
/// assert_eq!(clock.now_millis(), 1_700_000_000_250);
/// ```
#[derive(Debug)]
pub struct ManualClock {
    now: AtomicU64,
    waits: Mutex<Vec<u64>>,
}

impl ManualClock {
    /// A clock reading `millis`.
    #[must_use]
    pub fn at(millis: u64) -> Self {
        Self {
            now: AtomicU64::new(millis),
            waits: Mutex::new(Vec::new()),
        }
    }

    /// Moves the clock forward by `millis` — how a test lets a cool-down expire.
    pub fn advance(&self, millis: u64) {
        self.now.fetch_add(millis, Ordering::SeqCst);
    }

    /// Sets the clock to `millis`.
    pub fn set(&self, millis: u64) {
        self.now.store(millis, Ordering::SeqCst);
    }

    /// Every wait requested so far, in order — the observable backoff schedule.
    #[must_use]
    pub fn waits(&self) -> Vec<u64> {
        self.waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many waits were requested.
    #[must_use]
    pub fn wait_count(&self) -> usize {
        self.waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl Clock for ManualClock {
    fn now_millis(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn wait(&self, millis: u64) -> BoxFuture<'_, ()> {
        self.waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(millis);
        self.advance(millis);
        Box::pin(async {})
    }
}
