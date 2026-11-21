//! The counter the SLA bumps on every automatic approval.
//!
//! An approval by the clock is rare and worth watching, so the service reports each one to a
//! [`SlaMetrics`] port. The host wires the port to its metrics registry; the reference
//! [`CountingMetrics`] is a plain counter for tests.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::task::TaskId;

/// Where SLA counters go.
pub trait SlaMetrics: Send + Sync {
    /// One task was approved automatically by its SLA.
    fn auto_approved(&self, task: &TaskId);
}

/// The default: counts nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMetrics;

impl SlaMetrics for NoMetrics {
    fn auto_approved(&self, _task: &TaskId) {}
}

/// A counter of automatic approvals.
#[derive(Debug, Default)]
pub struct CountingMetrics {
    auto_approved: AtomicU64,
}

impl CountingMetrics {
    /// A counter at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many automatic approvals were reported.
    #[must_use]
    pub fn auto_approved_total(&self) -> u64 {
        self.auto_approved.load(Ordering::Relaxed)
    }
}

impl SlaMetrics for CountingMetrics {
    fn auto_approved(&self, _task: &TaskId) {
        self.auto_approved.fetch_add(1, Ordering::Relaxed);
    }
}
