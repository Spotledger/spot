//! Bulk export and incremental extraction (WP-INT-03.T1).
//!
//! This is the API channel for getting large amounts of data **out** of the platform: a
//! permission-filtered, field-guarded, keyset-paged stream of a DocType to CSV or NDJSON, a
//! signed resumable cursor, and a change-feed based incremental extract with tombstones.
//!
//! # The shape
//!
//! 1. [`ExportSpec`] names the DocType, the columns, the filter, the order and the format.
//! 2. [`ExportPlan::build`] checks the **export** right ([`ExportAuthority`]), the no-inference
//!    rule for filters and sorts, the [`Channel::Export`](spot_field_guard::Channel) columns,
//!    and ANDs the principal's read filter. It carries a stable digest.
//! 3. [`ExportStream::next_chunk`] yields bounded chunks by **keyset** paging on `(order, id)`,
//!    each page its own short read, so no transaction is held and no dataset is held in memory.
//!    [`ExportStream::cursor`] returns an opaque, signed [`Cursor`]; [`ExportStream::resume`]
//!    continues exactly after the last delivered row.
//! 4. [`IncrementalBatch::extract`] reads the change feed from a [`Watermark`], coalescing each
//!    changed document to its latest state, and reports deletions as tombstones. The new
//!    watermark is returned only after the final chunk.
//!
//! # Safety and bounds
//!
//! - The field guard is the one enforcement point: a column the caller may not export is an
//!   error naming the path, never silently dropped; a column that may leave only masked is
//!   exported masked.
//! - No `OFFSET` paging, no transaction across chunks, no reading all rows before sending.
//! - [`BulkLimits`], [`BulkCoordinator`], [`Telemetry`] and [`ExportAuditSink`] bound the work,
//!   enforce concurrency and rate, report counts, and record start/finish/cancel — never a
//!   value.
//!
//! The curated BI reporting views (`spot-bi-views`) are a different channel for BI tools; this
//! crate is the API channel and never reads them.

mod cursor;
mod error;
pub mod format;
mod incremental;
mod limits;
mod page;
mod plan;
mod spec;
mod stream;

pub use cursor::{CURSOR_VERSION, Cursor, CursorBody, CursorKey};
pub use error::{
    BulkError, BulkResult, CURSOR_INVALID, EXPORT_DENIED, LIMIT, SPEC_INVALID, STORAGE,
    WATERMARK_EXPIRED,
};
pub use format::Format;
pub use incremental::{
    BackendChangeReader, BatchChunk, ChangeReader, IncrementalBatch, IncrementalSpec, Watermark,
};
pub use limits::{
    BulkCoordinator, BulkLimits, ExportAuditEvent, ExportAuditSink, ExportLease, ExportPhase,
    NoopAuditSink, NoopCoordinator, NoopTelemetry, Progress, Telemetry,
};
pub use page::{BackendPageReader, BoxFuture, PageReader, after_predicate, after_predicate_exact};
pub use plan::{
    AllowAllExport, ChildPart, DenyAllExport, ExportAuthority, ExportPlan, PlanColumn, child_part,
};
pub use spec::{
    CHILD_ALIAS, ChildTable, EXPORT_ALIAS, ExportSpec, FieldSet, IDX_COLUMN, OrderKey,
    PARENT_COLUMN, Part,
};
pub use stream::{Chunk, ExportStream, StreamContext};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-bulk-data";
