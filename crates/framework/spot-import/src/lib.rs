//! The data import engine's front half (WP-DOC-09.T1, issue #534).
//!
//! Reads rows from a CSV or XLSX file, maps the file's columns onto a DocType's fields through
//! the effective metadata, converts every cell to the field's [`Value`] type (money and
//! quantities as exact [`Decimal`]s, never through floating point — ADR-12), groups parent and
//! child rows, checks each finished document with `spot_document::validate::check`, and reports
//! every problem against the **source row, column and cell text** so the user can fix the file
//! and upload it again.
//!
//! Nothing here talks to storage itself: [`apply`] is the engine's back half, which hands the
//! documents to a host through the app API's [`DocumentService`](spot_app_api::context::DocumentService).
//! Memory stays bounded: one row at a time from the file, one parent (plus its child rows) at a
//! time out of the engine, with only the user-configurable limits, the header and the
//! shared-string table as file-global state.
//!
//! # The pipeline
//!
//! 1. [`source`]: a [`source::RowSource`] streams [`source::Row`]s (1-based numbers, cells per
//!    column) from a CSV or XLSX file.
//! 2. [`mapping`]: a [`mapping::MappingPlan`] binds columns to fields before any row runs;
//!    `preflight::preflight` reports unmapped required fields and unused columns first.
//! 3. [`convert`]: text to [`Value`](spot_app_api::prelude::Value) per field type, with an
//!    explicit date format list and no day/month guessing.
//! 4. [`group`]: consecutive rows fold into one parent plus child rows via a group key
//!    (brief item 3), with `import.orphan_row` for children without a parent.
//! 5. [`stream`]: `stream::import_stream` yields [`stream::ImportItem::{Document, Error, Fatal}`] one
//!    parent at a time, validating each parent with `spot_document`.
//! 6. [`report`]: [`report::ImportReport`] keeps up to N full rows per code plus counts, and
//!    serialises to CSV for the fix-and-reupload round trip.
//! 7. [`apply`]: `apply::apply_import` writes the stream through a host's `DocumentService`
//!    (insert, update by key, upsert, or a dry run), one engine call per document, with
//!    idempotency, progress, cancellation and the all-or-nothing policy; `rollback_created`
//!    deletes the drafts a stopped run created.
//!
//! # Errors
//!
//! Whole-file failures are [`ImportError`]s with stable `import.*` codes; row and cell problems
//! are [`report::ImportRowError`]s in the report. Codes the engine raises, other than the
//! [`error::ImportError`] ones: `import.unmapped_column`, `import.missing_required_mapping`,
//! `import.unknown_import_field`, `import.bad_value`, `import.orphan_row`, `import.no_mapping`.
//!
//! [`ImportDocument`]: stream::ImportDocument
//! [`Decimal`]: spot_app_api::prelude::Decimal

/// The engine's back half: applying the validated stream through a host's `DocumentService`.
pub mod apply;
pub mod convert;
pub mod csv;
pub mod error;
pub mod group;
pub mod mapping;
/// Preflight: what the plan did not map, reported before any row is read (brief item 2).
pub mod preflight;
/// The import report: up to N full entries per code, counts, CSV serialisation and the import policy.
pub mod report;
pub mod source;
/// The engine stream: rows in, one [`ImportItem`](stream::ImportItem) (document or report
/// entry) per parent out.
pub mod stream;
/// A test DocType set (customers, posted sales invoices with items), `cfg(test)` only.
#[cfg(test)]
pub(crate) mod testmeta;
pub mod xlsx;

pub use apply::{
    ApplyMode, ApplyOptions, ApplyOutcome, ApplySummary, CancelToken, CreatedDocument, KeyIndex,
    MemorySpool, NoProgress, PermissionProbe, ProgressSink, RollbackOutcome, Spool, SpoolEntry,
    SpoolFull, StopReason, apply_import, rollback_created,
};
pub use csv::CsvSource;
pub use error::ImportError;
pub use mapping::{ImportPreflight, MappingPlan, Override, Target};
pub use report::{ImportPolicy, ImportReport, ImportRowError};
pub use source::{AnySource, Cell, Limits, Row, RowSource};
pub use stream::{ImportDocument, ImportItem, import_stream};
pub use xlsx::{Sheet, XlsxSource};
