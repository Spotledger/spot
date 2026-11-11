//! Per-connector monitoring: bounded-label counters and latency histograms through
//! `spot-telemetry`'s Prometheus recorder, a last-error record, and a circuit breaker per
//! connector per tenant.
//!
//! The breaker follows the resilience kit's shape (ADR-072): `Closed` counts consecutive
//! failures, opens past a threshold for a fixed window, then admits a bounded number of
//! `HalfOpen` probes. Time is always an argument.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::model::ConnectorId;

/// Counter: connector calls, labelled `connector`, `kind` and `outcome`. Every label is
/// bounded.
pub const CALLS_TOTAL: &str = "spot_connector_calls_total";
/// Histogram: connector call duration in seconds, labelled `connector` and `kind`.
pub const CALL_DURATION_SECONDS: &str = "spot_connector_call_duration_seconds";
/// Counter: calls refused because the breaker was open, labelled `connector`.
pub const CIRCUIT_OPEN_TOTAL: &str = "spot_connector_circuit_open_total";
/// Counter: calls a connector made to host imports, labelled `connector`.
pub const HOST_CALLS_TOTAL: &str = "spot_connector_host_calls_total";

/// Bucket upper bounds, in seconds, of [`CALL_DURATION_SECONDS`]: 1 ms to 30 s.
pub const DURATION_BUCKETS_SECONDS: [f64; 10] =
    [0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0];

/// The breaker's tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorConfig {
    /// Consecutive failures that open the breaker.
    pub failure_threshold: u32,
    /// How long the breaker stays open, in seconds.
    pub open_secs: u64,
    /// How many probes pass when the breaker is half-open.
    pub half_open_probes: u32,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_secs: 30,
            half_open_probes: 2,
        }
    }
}

/// Why a call was refused admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The breaker is open; a probe is allowed after the given seconds.
    CircuitOpen {
        /// Seconds until the next probe.
        retry_after_secs: u64,
    },
}

/// The breaker's state, for reporting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakerState {
    /// Normal operation.
    Closed,
    /// Refusing until the window elapses.
    Open,
    /// A limited number of probes pass.
    HalfOpen,
}

/// A connector's counters, for reporting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Calls that reached the guest.
    pub calls: u64,
    /// Calls that failed.
    pub failures: u64,
    /// The last error, secret-free.
    pub last_error: Option<String>,
    /// The breaker's state.
    pub breaker: BreakerState,
}

#[derive(Clone, Copy, Debug)]
enum Breaker {
    Closed { failures: u32 },
    Open { until_micros: i64 },
    HalfOpen { probes_left: u32 },
}

#[derive(Clone, Debug)]
struct State {
    breaker: Breaker,
    calls: u64,
    failures: u64,
    last_error: Option<String>,
}

struct Inner {
    config: MonitorConfig,
    states: Mutex<HashMap<(String, String), State>>,
}

/// The monitor: one breaker and one set of counters per `(connector, tenant)`.
#[derive(Clone)]
pub struct ConnectorMonitor {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ConnectorMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectorMonitor").finish_non_exhaustive()
    }
}

impl ConnectorMonitor {
    /// A monitor with `config`.
    pub fn new(config: MonitorConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                states: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), State>> {
        self.inner
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Admits one call, or refuses it because the breaker is open.
    pub fn acquire(
        &self,
        connector: &ConnectorId,
        tenant: &str,
        now_micros: i64,
    ) -> Result<Permit, Refusal> {
        let key = (connector.as_str().to_owned(), tenant.to_owned());
        let mut states = self.lock();
        let state = states.entry(key.clone()).or_insert_with(|| State {
            breaker: Breaker::Closed { failures: 0 },
            calls: 0,
            failures: 0,
            last_error: None,
        });
        match state.breaker {
            Breaker::Open { until_micros } => {
                if now_micros < until_micros {
                    let retry =
                        ((until_micros - now_micros).saturating_add(999_999) / 1_000_000) as u64;
                    metrics::counter!(CIRCUIT_OPEN_TOTAL, "connector" => key.0.clone())
                        .increment(1);
                    return Err(Refusal::CircuitOpen {
                        retry_after_secs: retry,
                    });
                }
                state.breaker = Breaker::HalfOpen {
                    probes_left: self.inner.config.half_open_probes,
                };
            }
            Breaker::Closed { .. } => {}
            Breaker::HalfOpen { .. } => {}
        }
        if let Breaker::HalfOpen { probes_left } = state.breaker {
            if probes_left == 0 {
                metrics::counter!(CIRCUIT_OPEN_TOTAL, "connector" => key.0.clone()).increment(1);
                return Err(Refusal::CircuitOpen {
                    retry_after_secs: self.inner.config.open_secs,
                });
            }
            state.breaker = Breaker::HalfOpen {
                probes_left: probes_left - 1,
            };
        }
        state.calls = state.calls.saturating_add(1);
        drop(states);
        Ok(Permit {
            inner: Arc::clone(&self.inner),
            connector: connector.as_str().to_owned(),
            tenant: tenant.to_owned(),
        })
    }

    /// Whether the breaker is currently open for `(connector, tenant)`.
    pub fn is_open(&self, connector: &ConnectorId, tenant: &str, now_micros: i64) -> bool {
        let states = self.lock();
        matches!(
            states.get(&(connector.as_str().to_owned(), tenant.to_owned())),
            Some(State {
                breaker: Breaker::Open { until_micros },
                ..
            }) if now_micros < *until_micros
        )
    }

    /// The counters for `(connector, tenant)`.
    pub fn stats(&self, connector: &ConnectorId, tenant: &str) -> Stats {
        let states = self.lock();
        match states.get(&(connector.as_str().to_owned(), tenant.to_owned())) {
            Some(state) => Stats {
                calls: state.calls,
                failures: state.failures,
                last_error: state.last_error.clone(),
                breaker: match state.breaker {
                    Breaker::Closed { .. } => BreakerState::Closed,
                    Breaker::Open { .. } => BreakerState::Open,
                    Breaker::HalfOpen { .. } => BreakerState::HalfOpen,
                },
            },
            None => Stats {
                calls: 0,
                failures: 0,
                last_error: None,
                breaker: BreakerState::Closed,
            },
        }
    }

    /// Records the latency of one call, in microseconds, for the histogram.
    pub fn record_latency(&self, connector: &ConnectorId, kind: &'static str, micros: u64) {
        metrics::histogram!(
            CALL_DURATION_SECONDS,
            "connector" => connector.as_str().to_owned(),
            "kind" => kind
        )
        .record(Duration::from_micros(micros).as_secs_f64());
    }

    /// Records one host-import call for the counter.
    pub fn record_host_call(&self, connector: &ConnectorId) {
        metrics::counter!(HOST_CALLS_TOTAL, "connector" => connector.as_str().to_owned())
            .increment(1);
    }
}

/// One admitted call. Recording its outcome updates the breaker; dropping it without a record
/// counts as a failure.
pub struct Permit {
    inner: Arc<Inner>,
    connector: String,
    tenant: String,
}

impl Permit {
    /// Records success: the breaker closes and the counters advance.
    pub fn record_success(&self, kind: &'static str) {
        let mut states = self
            .inner
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = states.get_mut(&(self.connector.clone(), self.tenant.clone())) {
            state.breaker = Breaker::Closed { failures: 0 };
        }
        finish_metric(&self.connector, kind, "ok");
    }

    /// Records failure: the counters advance and the breaker may open.
    pub fn record_failure(
        &self,
        error: &crate::ConnectorError,
        kind: &'static str,
        now_micros: i64,
    ) {
        let threshold = self.inner.config.failure_threshold;
        let open_secs = self.inner.config.open_secs;
        let mut states = self
            .inner
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = states.get_mut(&(self.connector.clone(), self.tenant.clone())) {
            state.failures = state.failures.saturating_add(1);
            state.last_error = Some(format!("{}: {error}", error.code()));
            let failures = match state.breaker {
                Breaker::Closed { failures } => failures.saturating_add(1),
                Breaker::HalfOpen { .. } => threshold,
                Breaker::Open { .. } => return,
            };
            state.breaker = if failures >= threshold {
                Breaker::Open {
                    until_micros: now_micros
                        .saturating_add((open_secs as i64).saturating_mul(1_000_000)),
                }
            } else {
                Breaker::Closed { failures }
            };
        }
        finish_metric(&self.connector, kind, "error");
    }
}

fn finish_metric(connector: &str, kind: &'static str, outcome: &'static str) {
    metrics::counter!(
        CALLS_TOTAL,
        "connector" => connector.to_owned(),
        "kind" => kind,
        "outcome" => outcome
    )
    .increment(1);
}
