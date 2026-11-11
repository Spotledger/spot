//! The resilience pieces a session needs, in the shape of ADR-072's kit.
//!
//! ADR-072 fixes the kit (deadline, retry, breaker, bulkhead) in `spot-core`; that work package
//! is not built yet, and ADR-072 says tasks already in flight keep local logic behind one
//! private implementation. So:
//!
//! * [`Deadline`] and [`RetryPolicy`] are reused from the state-machine crate
//!   (`spot_sync_protocol::resilience`), which already carries the ADR's shapes.
//! * [`CircuitBreaker`] is the one piece the state-machine crate does not have; it is local
//!   here, clock-driven and in memory, per named destination (the hub endpoint), as ADR-072
//!   section 4 describes.
//! * [`JitterSource`] and [`Sleeper`] are injected, so a test or the simulator is deterministic
//!   and nothing reads an ambient random source or the wall clock.
//!
//! `TODO(WP-REL-04)`: migrate to `spot-core::resilience` when WP-REL-06 lands. This module is
//! the only place that changes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::BoxFuture;
use crate::error::{Error, Result};

pub use spot_sync_protocol::resilience::{Deadline, RetryPolicy};

/// A source of jitter for a backoff, in parts per thousand (`0..=1000`).
///
/// Injected, never ambient: the same seed replays the same delays, so a failing retry sequence
/// is reproducible.
pub trait JitterSource: Send + Sync + 'static {
    /// The next jitter value, in `0..=1000`; `1000` means no jitter.
    fn permille(&mut self) -> u64;
}

/// A deterministic jitter source: xorshift64*, so a seed replays exactly.
#[derive(Clone, Debug)]
pub struct SeededJitter {
    state: u64,
}

impl SeededJitter {
    /// A source from a seed (never zero after seeding).
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }
}

impl JitterSource for SeededJitter {
    fn permille(&mut self) -> u64 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state
            .wrapping_mul(0x2545_F491_4F6C_DD1D)
            .wrapping_rem(1_001)
    }
}

/// A jitter source that never jitters (`1000` every time), for a deterministic full backoff.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoJitter;

impl JitterSource for NoJitter {
    fn permille(&mut self) -> u64 {
        1_000
    }
}

/// Where a retry waits between attempts.
///
/// Injected so a test never really sleeps: [`NoopSleeper`] returns at once, and a test sleeper
/// can advance a manual clock so the deadline is exercised deterministically.
pub trait Sleeper: Send + Sync + 'static {
    /// Resolves after `duration`. Cancellation-safe: dropping the future loses nothing.
    fn sleep(&self, duration: Duration) -> BoxFuture<()>;
}

/// A sleeper that returns immediately; the loopback and deterministic tests use it.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSleeper;

impl Sleeper for NoopSleeper {
    fn sleep(&self, _duration: Duration) -> BoxFuture<()> {
        Box::pin(std::future::ready(()))
    }
}

/// A sleeper backed by the Tokio timer, for a real socket client.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// When a destination's breaker opens and how it recovers (ADR-072 section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive failures that open the breaker; at least one.
    pub failure_threshold: u32,
    /// How long the breaker stays open before a probe.
    pub open_for: Duration,
    /// How many probes a half-open breaker admits.
    pub half_open_probes: u32,
}

impl BreakerConfig {
    /// A configuration; fails on a zero threshold.
    pub fn new(failure_threshold: u32, open_for: Duration, half_open_probes: u32) -> Result<Self> {
        if failure_threshold == 0 {
            return Err(Error::invalid_request(
                "a breaker's failure threshold is at least one",
            ));
        }
        Ok(Self {
            failure_threshold,
            open_for,
            half_open_probes,
        })
    }
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_for: Duration::from_secs(30),
            half_open_probes: 1,
        }
    }
}

/// One destination's breaker state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Breaker {
    /// Normal operation; counts consecutive failures.
    Closed { failures: u32 },
    /// Refusing until the monotonic instant `until`.
    Open { until: Duration },
    /// A limited number of probes pass.
    HalfOpen { probes_left: u32 },
}

/// The shared breaker state.
#[derive(Debug)]
struct Inner {
    config: BreakerConfig,
    destinations: Mutex<HashMap<String, Breaker>>,
}

/// A per-destination circuit breaker, in memory, driven by a monotonic `now` argument.
///
/// Clone it to share one breaker across a client's calls; every clone sees the same state.
#[derive(Clone, Debug)]
pub struct CircuitBreaker {
    inner: Arc<Inner>,
}

impl CircuitBreaker {
    /// A breaker with the given policy.
    pub fn new(config: BreakerConfig) -> Result<Self> {
        let config = BreakerConfig::new(
            config.failure_threshold,
            config.open_for,
            config.half_open_probes,
        )?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                destinations: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Admits one call to `destination` at monotonic `now`, or refuses with `sync_http.circuit_open`.
    ///
    /// A breaker that has been open long enough moves to half-open and admits a probe; a
    /// half-open breaker with no probes left refuses.
    pub fn acquire(&self, destination: &str, now: Duration) -> Result<Permit> {
        let mut destinations = self.inner.lock();
        let state = destinations
            .entry(destination.to_owned())
            .or_insert(Breaker::Closed { failures: 0 });
        match *state {
            Breaker::Open { until } => {
                if now < until {
                    return Err(Error::circuit_open(destination, until.saturating_sub(now)));
                }
                *state = Breaker::HalfOpen {
                    probes_left: self.inner.config.half_open_probes,
                };
            }
            Breaker::Closed { .. } | Breaker::HalfOpen { .. } => {}
        }
        if let Breaker::HalfOpen { probes_left } = *state {
            if probes_left == 0 {
                return Err(Error::circuit_open(destination, self.inner.config.open_for));
            }
            *state = Breaker::HalfOpen {
                probes_left: probes_left.saturating_sub(1),
            };
        }
        Ok(Permit {
            inner: Arc::clone(&self.inner),
            destination: destination.to_owned(),
        })
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Breaker>> {
        self.destinations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One admitted call; recording its outcome updates the destination's breaker.
///
/// The permit holds a handle to the shared state, so it is `Send` and may be held across an
/// await. Dropping it without recording leaves the breaker as it was.
#[derive(Debug)]
pub struct Permit {
    inner: Arc<Inner>,
    destination: String,
}

impl Permit {
    /// The destination this permit was admitted for.
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// Records a successful call: the breaker closes.
    pub fn record_success(&self) {
        let mut destinations = self.inner.lock();
        if let Some(state) = destinations.get_mut(&self.destination) {
            *state = Breaker::Closed { failures: 0 };
        }
    }

    /// Records a failed call at monotonic `now`: the breaker may open.
    pub fn record_failure(&self, now: Duration) {
        let threshold = self.inner.config.failure_threshold;
        let open_for = self.inner.config.open_for;
        let mut destinations = self.inner.lock();
        if let Some(state) = destinations.get_mut(&self.destination) {
            let failures = match *state {
                Breaker::Closed { failures } => failures.saturating_add(1),
                Breaker::HalfOpen { .. } => threshold,
                Breaker::Open { .. } => return,
            };
            *state = if failures >= threshold {
                Breaker::Open {
                    until: now.saturating_add(open_for),
                }
            } else {
                Breaker::Closed { failures }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let mut a = SeededJitter::new(7);
        let mut b = SeededJitter::new(7);
        for _ in 0..20 {
            let value = a.permille();
            assert!(value <= 1_000);
            assert_eq!(value, b.permille());
        }
        assert_eq!(NoJitter.permille(), 1_000);
    }

    #[test]
    fn the_breaker_opens_after_the_threshold_then_recovers() {
        let breaker =
            CircuitBreaker::new(BreakerConfig::new(1, Duration::from_secs(10), 1).unwrap())
                .unwrap();
        let now = Duration::from_secs(100);
        let permit = breaker.acquire("hub", now).unwrap();
        permit.record_failure(now);
        assert_eq!(
            breaker.acquire("hub", now).unwrap_err().code,
            crate::error::CIRCUIT_OPEN
        );
        // After the open period a probe passes, and a success closes the breaker.
        let probe = breaker
            .acquire("hub", now + Duration::from_secs(11))
            .unwrap();
        probe.record_success();
        assert!(
            breaker
                .acquire("hub", now + Duration::from_secs(11))
                .is_ok()
        );
    }

    #[test]
    fn a_failed_probe_reopens_the_breaker() {
        let breaker =
            CircuitBreaker::new(BreakerConfig::new(1, Duration::from_secs(10), 1).unwrap())
                .unwrap();
        let now = Duration::ZERO;
        breaker.acquire("hub", now).unwrap().record_failure(now);
        let probe = breaker.acquire("hub", Duration::from_secs(11)).unwrap();
        probe.record_failure(Duration::from_secs(11));
        assert_eq!(
            breaker
                .acquire("hub", Duration::from_secs(11))
                .unwrap_err()
                .code,
            crate::error::CIRCUIT_OPEN
        );
    }

    #[test]
    fn destinations_are_isolated() {
        let breaker =
            CircuitBreaker::new(BreakerConfig::new(1, Duration::from_secs(10), 1).unwrap())
                .unwrap();
        breaker
            .acquire("a", Duration::ZERO)
            .unwrap()
            .record_failure(Duration::ZERO);
        assert!(breaker.acquire("b", Duration::ZERO).is_ok());
    }

    #[test]
    fn deadlines_only_tighten() {
        let deadline = Deadline::new(Duration::from_millis(10), Duration::from_millis(90));
        assert_eq!(deadline.at(), Duration::from_millis(100));
        assert!(deadline.is_expired(Duration::from_millis(100)));
        assert_eq!(
            deadline
                .child(Duration::from_millis(50), Duration::from_millis(10))
                .at(),
            Duration::from_millis(60)
        );
    }
}
