//! Cancellation of a running AI statement (WP-AI-05.T1).
//!
//! When a call is cancelled or its deadline passes, its database statement is cancelled
//! through a [`StatementCanceller`] and its admission permit is released. The trait is the
//! seam the `spot-postgres` adapter implements later with `pg_cancel_backend`; this crate
//! never talks to a database. [`RecordingCanceller`] is the in-memory implementation tests and
//! hosts use.

use std::sync::Mutex;

/// One running statement, identified by the admission's monotonically increasing sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StatementId(pub u64);

/// Cancels a running statement by id.
pub trait StatementCanceller: Send + Sync {
    /// Cancels the statement, if it is still running. Best effort and idempotent.
    fn cancel(&self, statement: StatementId);
}

/// A canceller that records every cancellation, for tests and for a host with no database yet.
#[derive(Debug, Default)]
pub struct RecordingCanceller {
    cancelled: Mutex<Vec<StatementId>>,
}

impl RecordingCanceller {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many cancellations were recorded.
    #[must_use]
    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// Whether `statement` was cancelled.
    #[must_use]
    pub fn contains(&self, statement: StatementId) -> bool {
        self.lock().contains(&statement)
    }

    /// Empties the recorder.
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<StatementId>> {
        self.cancelled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl StatementCanceller for RecordingCanceller {
    fn cancel(&self, statement: StatementId) {
        self.lock().push(statement);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_recorder_keeps_every_cancellation() {
        let canceller = RecordingCanceller::new();
        canceller.cancel(StatementId(1));
        canceller.cancel(StatementId(1));
        assert_eq!(canceller.count(), 2);
        assert!(canceller.contains(StatementId(1)));
        canceller.clear();
        assert_eq!(canceller.count(), 0);
    }
}
