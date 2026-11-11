//! The bounds, the coordinator seam, the telemetry seam and the audit seam.
//!
//! Everything here is a trait the caller supplies, with a no-op fake for tests:
//!
//! - [`BulkLimits`]: rows and bytes per chunk, concurrent exports per tenant and per user, and
//!   the tenant's rows-per-second budget.
//! - [`BulkCoordinator`] / [`ExportLease`]: the enforcement point for the concurrency and rate
//!   bounds, wrapping `spot-coordinator`'s `Coordinator` in a host. The crate itself only
//!   defines the trait and a [`NoopCoordinator`].
//! - [`Telemetry`]: progress (rows and bytes) only — **never a value**.
//! - [`ExportAuditSink`]: every export start, finish and cancel — **never a value**.

use crate::error::BulkResult;

/// The bounds an export runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkLimits {
    /// The most rows in one chunk.
    pub rows_per_chunk: u32,
    /// The most bytes in one chunk.
    pub bytes_per_chunk: usize,
    /// The most exports one tenant may run at once.
    pub max_concurrent_per_tenant: u32,
    /// The most exports one user may run at once.
    pub max_concurrent_per_user: u32,
    /// The tenant's row budget per second.
    pub rows_per_second: u64,
}

impl Default for BulkLimits {
    fn default() -> Self {
        BulkLimits {
            rows_per_chunk: 1_000,
            bytes_per_chunk: 4 * 1024 * 1024,
            max_concurrent_per_tenant: 8,
            max_concurrent_per_user: 2,
            rows_per_second: 100_000,
        }
    }
}

/// One export's hold on the coordinator: it takes rows from the tenant's budget and releases
/// its concurrency slot when dropped.
pub trait ExportLease: Send {
    /// Takes `rows` from the tenant's row budget. A denial is `bulk.limit`.
    ///
    /// # Errors
    ///
    /// [`bulk.limit`](crate::error::LIMIT) when the budget is used up.
    fn take_rows(&self, rows: u64) -> BulkResult<()>;
}

/// The concurrency and rate enforcement point, wrapping `spot-coordinator` in a host.
pub trait BulkCoordinator: Send + Sync {
    /// Takes one export slot for `user` in `tenant`, or fails with `bulk.limit`.
    ///
    /// # Errors
    ///
    /// [`bulk.limit`](crate::error::LIMIT) when a concurrency bound is reached.
    fn acquire(
        &self,
        tenant: &str,
        user: &str,
        limits: &BulkLimits,
    ) -> BulkResult<Box<dyn ExportLease>>;
}

/// A coordinator that admits everything; a fake, and the default for a host that enforces
/// bounds elsewhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopCoordinator;

impl BulkCoordinator for NoopCoordinator {
    fn acquire(
        &self,
        _tenant: &str,
        _user: &str,
        _limits: &BulkLimits,
    ) -> BulkResult<Box<dyn ExportLease>> {
        Ok(Box::new(NoopLease))
    }
}

/// The lease [`NoopCoordinator`] hands out.
struct NoopLease;

impl ExportLease for NoopLease {
    fn take_rows(&self, _rows: u64) -> BulkResult<()> {
        Ok(())
    }
}

/// Export progress. Carries counts only, never a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Rows delivered so far.
    pub rows: u64,
    /// Bytes delivered so far.
    pub bytes: u64,
}

/// Where progress goes.
pub trait Telemetry: Send + Sync {
    /// Records progress. Called after every chunk.
    fn progress(&self, progress: &Progress);
}

/// Telemetry that records nothing; a fake and a safe default.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTelemetry;

impl Telemetry for NoopTelemetry {
    fn progress(&self, _progress: &Progress) {}
}

/// When an export audit record was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportPhase {
    /// The export started (or resumed).
    Start,
    /// The export delivered its last row.
    Finish,
    /// The export was dropped before finishing.
    Cancel,
}

/// One export audit record. Names who, which DocType, the column list, the filter digest, the
/// row count and the plan digest — **never a filter value and never a cell value**.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportAuditEvent {
    /// The acting user's name (empty for the system).
    pub who: String,
    /// The DocType.
    pub doctype: String,
    /// The exported columns (paths).
    pub columns: Vec<String>,
    /// A digest that names the filter without recording its values.
    pub filter_digest: String,
    /// The plan digest.
    pub plan_digest: String,
    /// Rows delivered so far.
    pub row_count: u64,
    /// The phase.
    pub phase: ExportPhase,
}

/// Where export audit records go. Takes the record by reference and answers at once.
pub trait ExportAuditSink: Send + Sync {
    /// Records one event.
    fn record(&self, event: &ExportAuditEvent);
}

/// An audit sink that records nothing; a fake and a safe default.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopAuditSink;

impl ExportAuditSink for NoopAuditSink {
    fn record(&self, _event: &ExportAuditEvent) {}
}
