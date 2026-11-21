//! Bulk lifecycle actions (WP-DOC-09.T6): one action applied to many existing documents.
//!
//! Users post twenty drafts, cancel a batch of wrong entries, set one field on a hundred
//! documents, delete a pile of drafts. [`apply_bulk`] does that through the app API's
//! `DocumentService`, one engine call per document, and reports what happened to each. The
//! import engine (`spot-import`) is the matching tool for **new** rows.
//!
//! # The contract in brief
//!
//! - **No batch transaction.** The engine has none: each document is its own transaction. Partial
//!   success is the normal result.
//! - **Per-document outcomes** ([`BulkStatus`]): `Succeeded`, `Skipped`, `Failed` (the engine's
//!   own code, class and message, untouched) and `NotRun`, in the order of the request.
//! - **Bounded**: at most `concurrency` calls in flight, at most `max_targets` documents,
//!   scheduling stops after `max_failures` failures or on cancellation.
//! - **Idempotent re-run**: see [`BulkOptions::batch_label`] and the README.
//! - **Dry run**: reads each document and reports what the action would do; never writes.
//!
//! The HTTP operation is not here; see the README.

mod apply;
mod command;
mod error;
mod exec;
mod types;

pub use apply::{apply_bulk, validate_request};
pub use error::{BulkError, codes};
pub use types::{
    BulkAction, BulkOptions, BulkOutcome, BulkResult, BulkStatus, BulkTarget, CancellationToken,
    DEFAULT_CONCURRENCY, DEFAULT_MAX_FAILURES, DEFAULT_MAX_TARGETS, DEFAULT_PROGRESS_EVERY,
    MAX_CONCURRENCY, MAX_LABEL_CHARS, NoProgress, ProgressSink, SkipReason, StopReason,
};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-bulk-actions";
