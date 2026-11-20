//! The request and result vocabulary of a bulk action.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value as Json;
use spot_app_api::ErrorClass;
use spot_app_api::document::DocumentPatch;
use spot_app_api::prelude::{RecordId, Version};

/// The most targets one request may name, unless `max_targets` is set lower.
pub const DEFAULT_MAX_TARGETS: usize = 10_000;
/// The default `max_failures`.
pub const DEFAULT_MAX_FAILURES: usize = 100;
/// The default number of calls in flight.
pub const DEFAULT_CONCURRENCY: usize = 4;
/// The most calls in flight a request may ask for.
pub const MAX_CONCURRENCY: usize = 16;
/// The longest batch label, in characters.
pub const MAX_LABEL_CHARS: usize = 64;
/// The default `progress_every`.
pub const DEFAULT_PROGRESS_EVERY: u64 = 50;

/// What to do to every target.
#[derive(Clone, Debug, PartialEq)]
pub enum BulkAction {
    /// Merge one patch into every target (`DocumentService::update`).
    Update(DocumentPatch),
    /// Post every target (`DocumentService::post`).
    Post,
    /// Cancel every target (`DocumentService::cancel`).
    Cancel,
    /// Delete every target (`DocumentService::delete`).
    Delete,
    /// Cancel every target and create its amended draft (`DocumentService::correct`).
    Correct,
    /// Call one whitelisted method on every target (`DocumentService::call_method`).
    Method {
        /// The method name, `[a-z0-9_]+`.
        name: String,
        /// The arguments, passed unchanged to every call.
        args: Json,
    },
}

impl BulkAction {
    /// The action's name as it enters the derived command id (a method includes its name).
    pub fn key(&self) -> String {
        match self {
            BulkAction::Update(_) => "update".to_owned(),
            BulkAction::Post => "post".to_owned(),
            BulkAction::Cancel => "cancel".to_owned(),
            BulkAction::Delete => "delete".to_owned(),
            BulkAction::Correct => "correct".to_owned(),
            BulkAction::Method { name, .. } => format!("method.{name}"),
        }
    }
}

/// One document to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkTarget {
    /// The DocType.
    pub doctype: String,
    /// The document.
    pub id: RecordId,
    /// `None`: load the document first and use its current version (a user's selection).
    /// `Some(v)`: a stale version is a per-document conflict.
    pub expected: Option<Version>,
}

impl BulkTarget {
    /// A target that acts on the document's current version.
    pub fn current(doctype: &str, id: RecordId) -> Self {
        Self {
            doctype: doctype.to_owned(),
            id,
            expected: None,
        }
    }
}

/// How the batch runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkOptions {
    /// The batch's identity, 1 to 64 characters, with no control character. Command ids derive
    /// from it: reuse it to re-run the same batch, change it for a new one.
    pub batch_label: String,
    /// Stop starting documents after this many failed ones; 0 means unlimited.
    pub max_failures: usize,
    /// Calls in flight at most, 1 to 16.
    pub concurrency: usize,
    /// More targets than this are refused before any call.
    pub max_targets: usize,
    /// Read only: report what each target would do and write nothing.
    pub dry_run: bool,
    /// A document already in the target state (posting a posted one) is skipped, not failed.
    pub skip_already_done: bool,
    /// The progress sink is called every this many documents (at least 1) and at the end.
    pub progress_every: u64,
}

impl BulkOptions {
    /// The defaults with the given label.
    pub fn new(batch_label: &str) -> Self {
        Self {
            batch_label: batch_label.to_owned(),
            max_failures: DEFAULT_MAX_FAILURES,
            concurrency: DEFAULT_CONCURRENCY,
            max_targets: DEFAULT_MAX_TARGETS,
            dry_run: false,
            skip_already_done: true,
            progress_every: DEFAULT_PROGRESS_EVERY,
        }
    }
}

/// Why a target was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The same document appeared earlier in the request.
    Duplicate,
    /// Posting a document that is already posted.
    AlreadyPosted,
    /// Cancelling a document that is already cancelled.
    AlreadyCancelled,
}

impl SkipReason {
    /// A stable short word for screens and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Duplicate => "duplicate",
            SkipReason::AlreadyPosted => "already_posted",
            SkipReason::AlreadyCancelled => "already_cancelled",
        }
    }
}

/// What happened to one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BulkStatus {
    /// The engine call succeeded. `version` is the document's version afterwards (the new
    /// draft's for `Correct`; `None` for `Delete`).
    Succeeded {
        /// The version afterwards.
        version: Option<Version>,
    },
    /// Dry run only: the call would be made and nothing found against it.
    WouldSucceed,
    /// Nothing to do.
    Skipped {
        /// Why.
        reason: SkipReason,
    },
    /// The call, or the load before it, was refused. `code`, `class` and `message` are the
    /// engine's, untouched.
    Failed {
        /// The engine's stable error code.
        code: String,
        /// The engine's error class.
        class: ErrorClass,
        /// The engine's message.
        message: String,
    },
    /// Never started: the batch stopped first.
    NotRun,
}

/// One target's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkResult {
    /// The target's position in the request.
    pub index: usize,
    /// The DocType.
    pub doctype: String,
    /// The document.
    pub id: RecordId,
    /// What happened.
    pub status: BulkStatus,
}

/// Why the batch stopped starting documents before the end of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// `max_failures` was reached.
    MaxFailures,
    /// The cancellation token was set.
    Cancelled,
}

/// The result of a batch. Partial success is the normal result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkOutcome {
    /// Targets whose call succeeded.
    pub succeeded: usize,
    /// Dry run: targets that would succeed.
    pub would_succeed: usize,
    /// Targets skipped (duplicates, already done).
    pub skipped: usize,
    /// Targets refused.
    pub failed: usize,
    /// Targets never started.
    pub not_run: usize,
    /// One result per target, in the order of the request.
    pub results: Vec<BulkResult>,
    /// Why scheduling stopped early, if it did.
    pub stopped: Option<StopReason>,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// Told how far the batch is. Called from the batch's own task, never concurrently with itself.
pub trait ProgressSink: Send + Sync {
    /// `done` of `total` targets have a result.
    fn progress(&self, done: u64, total: u64);
}

/// A sink that ignores progress.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn progress(&self, _done: u64, _total: u64) {}
}

/// Cancels a running batch. Clones share one flag.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels: no new document is started; the ones in flight finish.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether [`cancel`](Self::cancel) was called.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
