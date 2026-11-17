//! Acknowledgement latency and counters for the readiness text.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const RECENT: usize = 256;

/// What the gate counted.
#[derive(Default)]
pub struct Stats {
    recent: Mutex<VecDeque<u64>>,
    acked_local: AtomicU64,
    refused_halted: AtomicU64,
    outcome_unknown: AtomicU64,
    local_commit_durable: AtomicU64,
}

impl Stats {
    pub(crate) fn waited(&self, took: Duration) {
        let millis = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        let mut recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if recent.len() == RECENT {
            recent.pop_front();
        }
        recent.push_back(millis);
    }

    pub(crate) fn acked_local(&self) {
        self.acked_local.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn refused_halted(&self) {
        self.refused_halted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn outcome_unknown(&self) {
        self.outcome_unknown.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn local_commit_durable(&self) {
        self.local_commit_durable.fetch_add(1, Ordering::Relaxed);
    }

    /// The 99th percentile of the recent acknowledgement waits, in milliseconds (0 when none).
    #[must_use]
    pub fn p99_millis(&self) -> u64 {
        let recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut sorted: Vec<u64> = recent.iter().copied().collect();
        if sorted.is_empty() {
            return 0;
        }
        sorted.sort_unstable();
        let at = (sorted.len() * 99).div_ceil(100).saturating_sub(1);
        sorted.get(at).copied().unwrap_or(0)
    }

    /// Writes acknowledged on the local commit after the grace (degrade).
    #[must_use]
    pub fn acked_on_local(&self) -> u64 {
        self.acked_local.load(Ordering::Relaxed)
    }

    /// Writes refused before the handler ran (halt).
    #[must_use]
    pub fn refused_before_handler(&self) -> u64 {
        self.refused_halted.load(Ordering::Relaxed)
    }

    /// Committed writes that answered `journal.commit_outcome_unknown`.
    #[must_use]
    pub fn commit_outcome_unknown(&self) -> u64 {
        self.outcome_unknown.load(Ordering::Relaxed)
    }

    /// Writes acknowledged under `local` (the local commit is the durability).
    #[must_use]
    pub fn local_commit_durable_count(&self) -> u64 {
        self.local_commit_durable.load(Ordering::Relaxed)
    }
}
