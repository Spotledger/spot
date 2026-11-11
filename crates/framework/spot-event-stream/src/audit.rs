//! The audit trail: who registered a consumer, who acknowledged past which position, who
//! restarted a partition, and when a partition was parked.
//!
//! An [`AuditRecord`] carries **no event data**: only the actor, the consumer, the partition
//! and a position. The sink takes a record by value and answers without waiting, exactly as
//! the field guard's sinks do (ADR-070 section 12 rule 9), so the engine never blocks on a
//! slow journal.

use std::sync::Mutex;

use time::OffsetDateTime;

/// What happened, for the audit trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditRecord {
    /// A consumer was registered by an actor.
    Registered {
        /// The acting principal's name.
        actor: String,
        /// The consumer name.
        consumer: String,
        /// When.
        at: OffsetDateTime,
    },
    /// A partition's checkpoint was advanced by an acknowledgement.
    Acked {
        /// The acting principal's name (the consumer's owner).
        actor: String,
        /// The consumer name.
        consumer: String,
        /// The partition.
        partition: u32,
        /// The position acknowledged.
        position: u64,
        /// The stream generation of the acknowledged position.
        generation: u64,
        /// When.
        at: OffsetDateTime,
    },
    /// A partition was restarted from a stated position.
    Restarted {
        /// The acting principal's name.
        actor: String,
        /// The consumer name.
        consumer: String,
        /// The partition.
        partition: u32,
        /// The position it restarted from.
        position: u64,
        /// The stream generation it restarted from.
        generation: u64,
        /// When.
        at: OffsetDateTime,
    },
    /// A partition was parked because the consumer made no progress.
    Parked {
        /// The consumer name.
        consumer: String,
        /// The partition.
        partition: u32,
        /// When it was parked.
        at: OffsetDateTime,
    },
    /// A consumer was deleted by an actor.
    Deleted {
        /// The acting principal's name.
        actor: String,
        /// The consumer name.
        consumer: String,
        /// When.
        at: OffsetDateTime,
    },
}

/// Where audit records go. Takes the record by value and answers without waiting.
pub trait AuditSink: Send + Sync {
    /// Records one event.
    fn record(&self, record: AuditRecord);
}

/// A sink that keeps every record, for tests and the reference host.
#[derive(Debug, Default)]
pub struct RecordingAudit {
    records: Mutex<Vec<AuditRecord>>,
}

impl RecordingAudit {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AuditSink for RecordingAudit {
    fn record(&self, record: AuditRecord) {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
    }
}

/// A sink that drops everything: for hosts whose journal is wired elsewhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullAudit;

impl AuditSink for NullAudit {
    fn record(&self, _record: AuditRecord) {}
}
