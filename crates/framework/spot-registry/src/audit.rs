use crate::{AppName, Digest, Result};
use std::sync::Mutex;
/// Mutation kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditAction {
    /// New release indexed.
    Publish,
    /// Release removed from new install listings.
    Yank,
    /// Release security removal.
    Withdraw,
    /// Dual-authorized ownership change.
    Transfer,
}
/// Mutation evidence, without secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// Mutation kind.
    pub action: AuditAction,
    /// Authenticated actor ID.
    pub who: String,
    /// Affected app.
    pub app: AppName,
    /// Version, absent for transfer.
    pub version: Option<crate::Version>,
    /// Bundle or transfer commitment digest.
    pub digest: Digest,
    /// UTC seconds.
    pub at: i64,
    /// Removal or transfer reason.
    pub reason: String,
}
/// Injected synchronous audit destination. Must retain records and must not re-enter the
/// registry. Recording is infallible: durable adapters must provide their own delivery queue.
pub trait AuditSink: Send + Sync {
    /// Records a completed mutation.
    fn record(&self, record: AuditRecord);
}
/// In-memory audit destination for local use.
#[derive(Default)]
pub struct MemoryAuditSink {
    records: Mutex<Vec<AuditRecord>>,
}
impl MemoryAuditSink {
    /// Copies the recorded evidence.
    pub fn records(&self) -> Result<Vec<AuditRecord>> {
        Ok(self
            .records
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .clone())
    }
}
impl AuditSink for MemoryAuditSink {
    fn record(&self, record: AuditRecord) {
        // Retain evidence even if a prior caller panicked while holding this sink's lock.
        let mut records = match self.records.lock() {
            Ok(records) => records,
            Err(e) => e.into_inner(),
        };
        records.push(record);
    }
}
