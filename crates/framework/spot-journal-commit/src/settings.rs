//! Tuning of the driver. Times are milliseconds.

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
    /// How long a locally committed request may wait for its acknowledgement. The gate enforces
    /// the deadline from its own `GateConfig`; the driver uses this value only to decide when to
    /// wake, so it must equal the gate's `ack_deadline_millis`.
    pub ack_deadline_millis: u64,
}

impl Default for CommitSettings {
    fn default() -> Self {
        Self {
            group_window_millis: 5,
            max_batch: 256,
            retry_millis: 1_000,
            first_batch_id: 1,
            ack_deadline_millis: 30_000,
        }
    }
}
