//! The human-traffic load signal: how busy the site's **main** pool is, measured by the service
//! itself so AI admission shrinks when people wait.
//!
//! A sampler (run by the service) takes a connection from the main pool every `sample_ms`, timing
//! how long that took (the pool wait a person's request would see) and reading the number of
//! requests queued for a connection. The last readings are kept in a small ring; the signal
//! reports the latest wait, the queue depth and the p95 of the ring.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use spot_ai_isolation::{LoadSample, LoadSignal};

/// How many probes the p95 is taken over.
pub const RING: usize = 20;

/// The shared signal: written by the sampler, read at every admission.
#[derive(Default)]
pub struct HumanLoad {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    waits: VecDeque<Duration>,
    queue_depth: u32,
}

impl HumanLoad {
    /// Records one probe: the wait for a connection and the queue depth at that moment.
    pub fn record(&self, wait: Duration, queue_depth: u32) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.waits.len() == RING {
            inner.waits.pop_front();
        }
        inner.waits.push_back(wait);
        inner.queue_depth = queue_depth;
    }

    /// The current reading.
    #[must_use]
    pub fn current(&self) -> LoadSample {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(latest) = inner.waits.back().copied() else {
            return LoadSample::calm();
        };
        LoadSample {
            queue_depth: inner.queue_depth,
            pool_wait: latest,
            p95: p95(&inner.waits),
        }
    }
}

/// The 95th percentile of `values` by nearest rank (integer arithmetic only).
fn p95(values: &VecDeque<Duration>) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted: Vec<Duration> = values.iter().copied().collect();
    sorted.sort();
    // Nearest rank: ceil(0.95 * n) as an index, in integers.
    let rank = (sorted.len() * 95).div_ceil(100).max(1);
    sorted.get(rank - 1).copied().unwrap_or(Duration::ZERO)
}

impl LoadSignal for HumanLoad {
    fn sample(&self, _now: Duration) -> LoadSample {
        self.current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn no_probe_yet_is_calm() {
        assert_eq!(HumanLoad::default().current(), LoadSample::calm());
    }

    #[test]
    fn the_signal_reports_the_latest_wait_the_queue_and_the_ring_p95() {
        let load = HumanLoad::default();
        for n in 1..=20 {
            load.record(ms(n), 2);
        }
        load.record(ms(500), 7);
        let sample = load.current();
        assert_eq!(sample.pool_wait, ms(500));
        assert_eq!(sample.queue_depth, 7);
        // The ring holds 2..=20 and 500 (20 values): rank ceil(20 * 95 / 100) = 19.
        assert_eq!(sample.p95, ms(20));
    }

    #[test]
    fn the_ring_forgets_the_oldest_probe() {
        let load = HumanLoad::default();
        load.record(ms(900), 0);
        for _ in 0..RING {
            load.record(ms(1), 0);
        }
        assert_eq!(load.current().p95, ms(1));
    }
}
