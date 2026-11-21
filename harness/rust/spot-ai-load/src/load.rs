//! The interactive-load signal the admission control reads, built from what the human users
//! of this run actually experienced: their recent latencies, their pool waits and the number
//! of them waiting for a connection right now.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_ai_isolation::{LoadSample, LoadSignal};

use crate::stats::nearest_rank;

/// How many recent human operations the p95 is taken over.
const LATENCY_WINDOW: usize = 256;
/// How many recent pool waits the pool-wait reading is the maximum of.
const WAIT_WINDOW: usize = 64;

/// The human-traffic load signal.
pub struct HumanLoad {
    pool: Pool,
    latencies: Mutex<VecDeque<u64>>,
    waits: Mutex<VecDeque<u64>>,
}

impl HumanLoad {
    /// A signal reading the human `pool`.
    #[must_use]
    pub fn new(pool: Pool) -> Arc<Self> {
        Arc::new(Self {
            pool,
            latencies: Mutex::new(VecDeque::with_capacity(LATENCY_WINDOW)),
            waits: Mutex::new(VecDeque::with_capacity(WAIT_WINDOW)),
        })
    }

    /// Records one finished human operation.
    pub fn record(&self, latency: Duration, pool_wait: Duration) {
        push_bounded(&self.latencies, micros(latency), LATENCY_WINDOW);
        push_bounded(&self.waits, micros(pool_wait), WAIT_WINDOW);
    }
}

impl LoadSignal for HumanLoad {
    fn sample(&self, _now: Duration) -> LoadSample {
        let mut sorted: Vec<u64> = lock(&self.latencies).iter().copied().collect();
        sorted.sort_unstable();
        let wait = lock(&self.waits).iter().copied().max().unwrap_or(0);
        LoadSample {
            queue_depth: u32::try_from(self.pool.status().waiting).unwrap_or(u32::MAX),
            pool_wait: Duration::from_micros(wait),
            p95: Duration::from_micros(nearest_rank(&sorted, 95)),
        }
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn push_bounded(queue: &Mutex<VecDeque<u64>>, value: u64, cap: usize) {
    let mut q = lock(queue);
    if q.len() == cap {
        q.pop_front();
    }
    q.push_back(value);
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
