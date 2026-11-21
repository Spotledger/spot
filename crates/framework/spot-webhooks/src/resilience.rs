//! A small, local per-endpoint circuit breaker and bulkhead, so one dead endpoint cannot
//! consume the worker pool.
//!
//! TODO(WP-REL-04): migrate to the resilience kit. The kit (WP-REL-06) is unbuilt and ADR-072
//! is still proposed; ADR-072 says tasks already in flight keep local logic. This module has
//! the ADR's breaker states (`Closed`, `Open`, `HalfOpen`) and typed refusals behind one
//! private implementation, and is the only place that would change.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use spot_core::Clock;
use time::OffsetDateTime;

use crate::Error;
use crate::model::{BulkheadConfig, CircuitBreakerConfig, RateLimitConfig};

/// Why a delivery was refused admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The endpoint's breaker is open; a probe is allowed after the given seconds.
    CircuitOpen {
        /// Seconds until the next probe.
        retry_after_secs: u64,
    },
    /// The endpoint already has its maximum deliveries in flight.
    BulkheadFull,
    /// The endpoint's per-second rate limit is reached.
    RateLimited {
        /// Seconds until the next window.
        retry_after_secs: u64,
    },
}

/// The breaker's state for one endpoint (ADR-072's states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Breaker {
    /// Normal operation; counts consecutive failures.
    Closed { failures: u32 },
    /// Refusing until `until`.
    Open { until: OffsetDateTime },
    /// A limited number of probes pass.
    HalfOpen { probes_left: u32 },
}

#[derive(Debug)]
struct EndpointState {
    breaker: Breaker,
    in_flight: u32,
    /// The start of the current one-second window.
    window_start: OffsetDateTime,
    /// Deliveries admitted in the current window.
    window_count: u32,
}

struct Inner {
    clock: Arc<dyn Clock>,
    breaker: CircuitBreakerConfig,
    bulkhead: BulkheadConfig,
    rate: RateLimitConfig,
    endpoints: Mutex<HashMap<String, EndpointState>>,
}

/// The per-endpoint breaker and bulkhead; clone to share it across handlers.
#[derive(Clone)]
pub struct Resilience {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Resilience {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resilience").finish_non_exhaustive()
    }
}

impl Resilience {
    /// A breaker, bulkhead and rate limit over `clock`.
    pub fn new(
        clock: Arc<dyn Clock>,
        breaker: CircuitBreakerConfig,
        bulkhead: BulkheadConfig,
        rate: RateLimitConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                clock,
                breaker,
                bulkhead,
                rate,
                endpoints: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Admits one delivery to `endpoint`, or refuses it.
    ///
    /// # Errors
    ///
    /// [`Refusal::CircuitOpen`] when the breaker is open, [`Refusal::BulkheadFull`] when the
    /// endpoint's concurrency is already saturated.
    pub fn acquire(&self, endpoint: &str) -> Result<Permit, Refusal> {
        let now = self.inner.clock.now_utc();
        let mut endpoints = self.lock();
        let state = endpoints
            .entry(endpoint.to_owned())
            .or_insert_with(|| EndpointState {
                breaker: Breaker::Closed { failures: 0 },
                in_flight: 0,
                window_start: now,
                window_count: 0,
            });

        if let Breaker::Open { until } = state.breaker {
            if now < until {
                let secs = (until - now).whole_seconds().max(0) as u64;
                return Err(Refusal::CircuitOpen {
                    retry_after_secs: secs,
                });
            }
            state.breaker = Breaker::HalfOpen {
                probes_left: self.inner.breaker.half_open_probes,
            };
        }
        if let Breaker::HalfOpen { probes_left } = state.breaker
            && probes_left == 0
        {
            return Err(Refusal::CircuitOpen {
                retry_after_secs: self.inner.breaker.open_secs,
            });
        }
        if state.in_flight >= self.inner.bulkhead.max_concurrent {
            return Err(Refusal::BulkheadFull);
        }
        if self.inner.rate.per_second > 0 {
            if (now - state.window_start).whole_seconds() >= 1 {
                state.window_start = now;
                state.window_count = 0;
            }
            if state.window_count >= self.inner.rate.per_second {
                return Err(Refusal::RateLimited {
                    retry_after_secs: 1,
                });
            }
            state.window_count += 1;
        }

        state.in_flight += 1;
        if let Breaker::HalfOpen { probes_left } = state.breaker {
            state.breaker = Breaker::HalfOpen {
                probes_left: probes_left.saturating_sub(1),
            };
        }
        Ok(Permit {
            inner: self.inner.clone(),
            endpoint: endpoint.to_owned(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, EndpointState>> {
        self.inner
            .endpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One admitted delivery; dropping it releases the bulkhead slot.
pub struct Permit {
    inner: Arc<Inner>,
    endpoint: String,
}

impl Permit {
    /// Records a successful attempt: the breaker closes.
    pub fn record_success(&self) {
        let mut endpoints = self
            .inner
            .endpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = endpoints.get_mut(&self.endpoint) {
            state.breaker = Breaker::Closed { failures: 0 };
        }
    }

    /// Records a failed attempt: the breaker may open.
    pub fn record_failure(&self) {
        let now = self.inner.clock.now_utc();
        let threshold = self.inner.breaker.failure_threshold;
        let mut endpoints = self
            .inner
            .endpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = endpoints.get_mut(&self.endpoint) {
            let failures = match state.breaker {
                Breaker::Closed { failures } => failures.saturating_add(1),
                Breaker::HalfOpen { .. } => threshold,
                Breaker::Open { .. } => return,
            };
            state.breaker = if failures >= threshold {
                Breaker::Open {
                    until: now + time::Duration::seconds(self.inner.breaker.open_secs as i64),
                }
            } else {
                Breaker::Closed { failures }
            };
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut endpoints = self
            .inner
            .endpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = endpoints.get_mut(&self.endpoint) {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }
}

impl From<(String, Refusal)> for Error {
    fn from((endpoint, refusal): (String, Refusal)) -> Self {
        match refusal {
            Refusal::CircuitOpen { retry_after_secs } => Error::CircuitOpen {
                endpoint,
                retry_after_secs,
            },
            Refusal::BulkheadFull => Error::BulkheadFull { endpoint },
            Refusal::RateLimited { retry_after_secs } => {
                Error::DeliveryRateLimited { retry_after_secs }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_core::ManualClock;
    use std::time::Duration;

    fn harness(threshold: u32) -> (Resilience, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let resilience = Resilience::new(
            clock.clone(),
            CircuitBreakerConfig {
                failure_threshold: threshold,
                open_secs: 10,
                half_open_probes: 1,
            },
            BulkheadConfig { max_concurrent: 2 },
            RateLimitConfig::default(),
        );
        (resilience, clock)
    }

    #[test]
    fn breaker_opens_after_threshold_then_recovers() {
        let (resilience, clock) = harness(2);

        let permit = resilience.acquire("https://a.example").unwrap();
        permit.record_failure();
        drop(permit);
        let permit = resilience.acquire("https://a.example").unwrap();
        permit.record_failure();
        drop(permit);

        assert!(matches!(
            resilience.acquire("https://a.example"),
            Err(Refusal::CircuitOpen { .. })
        ));

        // After the open period, one probe passes; a success closes the breaker.
        clock.advance(Duration::from_secs(11));
        let permit = resilience.acquire("https://a.example").unwrap();
        permit.record_success();
        drop(permit);
        assert!(resilience.acquire("https://a.example").is_ok());
    }

    #[test]
    fn a_failed_probe_reopens_the_breaker() {
        let (resilience, clock) = harness(1);
        let permit = resilience.acquire("e").unwrap();
        permit.record_failure();
        drop(permit);
        clock.advance(Duration::from_secs(11));
        let permit = resilience.acquire("e").unwrap();
        permit.record_failure();
        drop(permit);
        assert!(matches!(
            resilience.acquire("e"),
            Err(Refusal::CircuitOpen { .. })
        ));
    }

    #[test]
    fn bulkhead_limits_concurrency_then_recovers() {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let resilience = Resilience::new(
            clock,
            CircuitBreakerConfig::default(),
            BulkheadConfig { max_concurrent: 1 },
            RateLimitConfig::default(),
        );
        let first = resilience.acquire("e").unwrap();
        assert!(matches!(
            resilience.acquire("e"),
            Err(Refusal::BulkheadFull)
        ));
        drop(first);
        assert!(resilience.acquire("e").is_ok());
    }

    #[test]
    fn endpoints_are_isolated() {
        let (resilience, _clock) = harness(1);
        let permit = resilience.acquire("a").unwrap();
        permit.record_failure();
        drop(permit);
        assert!(resilience.acquire("b").is_ok());
    }

    #[test]
    fn the_rate_limit_admits_a_fixed_number_per_window() {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let resilience = Resilience::new(
            clock.clone(),
            CircuitBreakerConfig::default(),
            BulkheadConfig { max_concurrent: 10 },
            RateLimitConfig { per_second: 2 },
        );
        assert!(resilience.acquire("e").is_ok());
        assert!(resilience.acquire("e").is_ok());
        assert!(matches!(
            resilience.acquire("e"),
            Err(Refusal::RateLimited { .. })
        ));
        clock.advance(Duration::from_secs(1));
        assert!(resilience.acquire("e").is_ok());
    }
}
