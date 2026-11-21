//! Tuning of the driver. Times are milliseconds.

use crate::error::CommitError;

/// Settings of one [`crate::CommitDriver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitSettings {
    /// How long the oldest queued request waits for company before its batch is shipped.
    pub group_window_millis: u64,
    /// The most requests one batch (one object-store round trip) holds.
    pub max_batch: usize,
    /// How long after a store failure the driver waits before it probes the store again.
    pub retry_millis: u64,
    /// The id of the first batch the driver sends; ids only ever grow. Must be after the last
    /// batch id the relay has accepted.
    pub first_batch_id: u64,
    /// How long a submitted request may wait for its `local_commit_durable` call. A request
    /// still unconfirmed after this time is treated as `local_commit_failed` and counted in
    /// `DriverStatus::expired_local_commits`. Must be at least the gate's `ack_deadline_millis`
    /// (the acknowledgement deadline itself is read from the gate's `GateConfig`, the only place
    /// it is configured).
    pub local_commit_deadline_millis: u64,
}

impl CommitSettings {
    /// Checks the settings against the gate's acknowledgement deadline.
    ///
    /// # Errors
    /// `invalid_setting` when `local_commit_deadline_millis` is below `ack_deadline_millis`.
    pub fn validate(&self, ack_deadline_millis: u64) -> Result<(), CommitError> {
        if self.local_commit_deadline_millis < ack_deadline_millis {
            return Err(CommitError::InvalidSetting {
                name: "local_commit_deadline_millis",
            });
        }
        Ok(())
    }
}

impl Default for CommitSettings {
    fn default() -> Self {
        Self {
            group_window_millis: 5,
            max_batch: 256,
            retry_millis: 1_000,
            first_batch_id: 1,
            local_commit_deadline_millis: 120_000,
        }
    }
}
