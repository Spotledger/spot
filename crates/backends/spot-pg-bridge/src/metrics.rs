//! The metric names the bridge exports, through the recorder `spot-telemetry` installs.
//!
//! The only label is `bridge`, the configured name (validated to a short identifier). No label
//! value is ever SQL, a parameter, a row or an error text.

use std::time::Duration;

/// Gauge: calls waiting for a slot.
pub const QUEUE_DEPTH: &str = "spot_pg_bridge_queue_depth";
/// Gauge: calls running.
pub const IN_FLIGHT: &str = "spot_pg_bridge_in_flight";
/// Histogram: seconds a call waited for a slot and a connection.
pub const WAIT_SECONDS: &str = "spot_pg_bridge_wait_seconds";
/// Histogram: seconds a call's closure ran.
pub const RUN_SECONDS: &str = "spot_pg_bridge_run_seconds";
/// Counter: calls refused because the queue was full.
pub const BUSY_TOTAL: &str = "spot_pg_bridge_busy_total";
/// Counter: calls that missed their deadline.
pub const TIMEOUT_TOTAL: &str = "spot_pg_bridge_timeout_total";
/// Counter: calls refused because the caller was inside an async runtime.
pub const WRONG_THREAD_TOTAL: &str = "spot_pg_bridge_wrong_thread_total";
/// Counter: running calls the bridge cancelled (a timeout or a shutdown).
pub const CANCELLED_TOTAL: &str = "spot_pg_bridge_cancelled_total";

/// Emits the bridge's metrics under one `bridge` label.
#[derive(Clone)]
pub(crate) struct Emitter {
    name: String,
}

impl Emitter {
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
        }
    }

    pub(crate) fn queue_depth(&self, n: usize) {
        metrics::gauge!(QUEUE_DEPTH, "bridge" => self.name.clone()).set(n as f64);
    }

    pub(crate) fn in_flight(&self, n: usize) {
        metrics::gauge!(IN_FLIGHT, "bridge" => self.name.clone()).set(n as f64);
    }

    pub(crate) fn waited(&self, d: Duration) {
        metrics::histogram!(WAIT_SECONDS, "bridge" => self.name.clone()).record(d.as_secs_f64());
    }

    pub(crate) fn ran(&self, d: Duration) {
        metrics::histogram!(RUN_SECONDS, "bridge" => self.name.clone()).record(d.as_secs_f64());
    }

    pub(crate) fn busy(&self) {
        metrics::counter!(BUSY_TOTAL, "bridge" => self.name.clone()).increment(1);
    }

    pub(crate) fn timeout(&self) {
        metrics::counter!(TIMEOUT_TOTAL, "bridge" => self.name.clone()).increment(1);
    }

    pub(crate) fn wrong_thread(&self) {
        metrics::counter!(WRONG_THREAD_TOTAL, "bridge" => self.name.clone()).increment(1);
    }

    pub(crate) fn cancelled(&self) {
        metrics::counter!(CANCELLED_TOTAL, "bridge" => self.name.clone()).increment(1);
    }
}
