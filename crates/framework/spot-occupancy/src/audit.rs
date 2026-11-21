//! The audit trail of occupancy operations, failures included.
//!
//! A refused claim rolls its transaction back, so nothing in the database records that someone
//! tried. The service therefore reports every finished operation to an [`AuditSink`], **outside**
//! any transaction (no external call inside one, ADR-51), on success and on failure alike. The
//! entry carries ids and a code, never range values, holders' names or payloads.

use std::sync::Mutex;

use spot_storage_contract::RecordId;
use time::OffsetDateTime;

/// What was attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// A tentative hold.
    Hold,
    /// A booking in one insert.
    BookNow,
    /// A confirm.
    Confirm,
    /// A release (cancel).
    Release,
    /// One sweeper batch.
    Sweep,
}

/// How it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It happened.
    Done,
    /// It was refused or failed, with the stable code (`occupancy.conflict`, ...).
    Refused(&'static str),
}

/// One audit entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// When it finished, by the service's clock.
    pub at: OffsetDateTime,
    /// What was attempted.
    pub action: Action,
    /// Who: `user:<id>`, `consumer:<id>`, `operator` or `platform`.
    pub actor: String,
    /// The unit, when the operation named one.
    pub unit: Option<RecordId>,
    /// The occupancy record, when there is one.
    pub record: Option<RecordId>,
    /// How it ended.
    pub outcome: Outcome,
}

/// Receives audit entries. Implementations must not block for long and must not fail the
/// operation: the entry is best effort at this seam; the durable chain is the sink's concern.
pub trait AuditSink: Send + Sync {
    /// Records one entry.
    fn record(&self, entry: AuditEntry);
}

/// Drops every entry. For tests that do not look at the audit.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAudit;

impl AuditSink for NoAudit {
    fn record(&self, _entry: AuditEntry) {}
}

/// Keeps entries in memory, for tests and for hosts that drain them.
#[derive(Debug, Default)]
pub struct MemoryAudit {
    entries: Mutex<Vec<AuditEntry>>,
}

impl MemoryAudit {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of the entries so far, in the order they were recorded.
    pub fn entries(&self) -> Vec<AuditEntry> {
        match self.entries.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl AuditSink for MemoryAudit {
    fn record(&self, entry: AuditEntry) {
        match self.entries.lock() {
            Ok(mut g) => g.push(entry),
            Err(poisoned) => poisoned.into_inner().push(entry),
        }
    }
}
