//! The import engine's back half (WP-DOC-09.T2, issue #1198): apply the validated stream to a
//! host through the app API's [`DocumentService`].
//!
//! [`crate::stream::import_stream`] turns rows into validated parents and row problems.
//! [`apply_import`] takes that stream and writes it: one engine call per document, in insert,
//! update-by-key or upsert mode, or in a dry run that writes nothing. Everything the user needs
//! to fix their file comes back as [`ImportRowError`]s in the report's own format (source row,
//! column, code), so the writer never invents a second error shape.
//!
//! # One document is one engine call
//!
//! The engine runs one transaction per top-level call (see `spot_document`'s engine docs) and
//! has no multi-document transaction. The writer therefore makes exactly one `create`,
//! `update` or `post` call per document, and never wraps several documents in one call. What
//! that costs, and what it buys:
//!
//! - A failure half-way leaves the documents written before it in place. The outcome says so
//!   ([`ApplyOutcome::PartiallyApplied`]) and lists exactly the drafts this run created, so the
//!   caller can call [`rollback_created`] to delete them. Documents this run *updated* are not
//!   rolled back: the engine has no undo, and restoring the old values would itself be a write
//!   the importer must not guess at.
//! - A draft that was already posted (when [`ApplyOptions::post_after_import`] is set) is never
//!   deleted: a posted document is a business fact. [`rollback_created`] reports it as needing
//!   a cancel instead ([`RollbackOutcome::needs_cancel`]).
//!
//! # Idempotency
//!
//! Each call gets an [`Invocation`] copy whose `command_id` is derived deterministically from
//! the **site, the actor, the target DocType, [`ApplyOptions::batch_label`] and the document's
//! first source row number**, together with the operation (`c` create, `u` update, `p` post,
//! `undo` rollback). The inputs are length-prefixed and hashed with SHA-256; the id keeps the
//! first 128 bits:
//!
//! ```text
//! impc-<32 hex digits>-42        create of the parent that starts at source row 42
//! ```
//!
//! The engine's command-id dedupe returns the first result for a command id it has already
//! committed, so re-running the same import after a crash returns the first run's documents
//! instead of creating duplicates. The derivation is stable across processes and restarts (no
//! clock, no randomness) and cannot collide by accident: two imports share keys only when they
//! share site, actor, DocType, label and row. The label is therefore the batch's identity and
//! must be unique per import (a job id, or a digest of the file's content); a real run with an
//! empty label, or with the old default `"import"`, is refused with
//! [`StopReason::Configuration`]. A dry run needs no label because it never sends a command id.
//!
//! A create whose result is a document that does not hold the imported values (a stored
//! command id from some other import, whatever the cause) is **never** counted as created: it is
//! a failed write with the code [`crate::error::IDEMPOTENCY_CONFLICT`].
//!
//! # All-or-nothing without a multi-document transaction
//!
//! [`ImportPolicy::AllOrNothing`] first drains the whole stream with no writes and **validates
//! everything a write could refuse for a reason the writer can see**, keeping each validated
//! document in a caller-supplied [`Spool`]:
//!
//! - the stream's own row problems (types, required fields, links);
//! - for the keyed modes, every key: present, not repeated in the file, and (for `Update`) found
//!   in the [`KeyIndex`] and loadable through [`DocumentService::load`];
//! - the caller's [`PermissionProbe`] (create, or update of the named document), when one is
//!   supplied.
//!
//! Only when the whole file passed with no row problem and no whole-file failure does the write
//! pass start, so a bad key in the last row of an `Update` file writes nothing. The write pass
//! plans each document again against the freshly loaded version.
//!
//! What the policy covers, and what it cannot:
//!
//! - **Covered:** a file with any problem the validation pass can see writes nothing at all.
//! - **Rolled back on a later failure:** the *drafts this run created*
//!   ([`ApplySummary::created_drafts`], through [`rollback_created`]).
//! - **Not rolled back:** documents this run *updated* (the engine has no undo and the importer
//!   will not guess old values), and posted documents (a business fact; cancel them).
//! - **Not atomic:** between the validation pass and the write pass another user may change a
//!   document, a key may disappear, or a permission may be revoked, and a crash leaves what
//!   already committed. If a write then fails anyway the run stops with
//!   [`ApplyOutcome::PartiallyApplied`]. There is no way to make the write pass atomic with the
//!   classes the app API provides.
//!
//! # Bounded memory
//!
//! The streaming policy ([`ImportPolicy::SkipBadRows`]) holds one document at a time plus the
//! ids of what it created. The all-or-nothing pass holds one document plus what the
//! [`Spool`] keeps; the default [`MemorySpool`] has a hard cap ([`DEFAULT_SPOOL_CAPACITY`]) and
//! refuses past it with the code [`crate::error::SPOOL_FULL`], writing nothing at all. A
//! disk-backed spool for very large all-or-nothing files is a follow-up.
//!
//! # Dry run
//!
//! [`ApplyMode::DryRun`] runs the same checks the validation pass of a real run does:
//! `to_document` (as the stream already did), the key checks, the engine `load` of the existing
//! document, and the caller's [`PermissionProbe`] for create or update, reporting refusals as
//! row errors. It reports what each row would do through [`ApplySummary::created`],
//! [`ApplySummary::updated`] and [`ApplySummary::skipped`], with [`ApplySummary::dry_run`] set
//! and nothing written.
//!
//! The app API's [`DocumentService`] has no permission probe of its own: a refusal otherwise
//! shows only when a write is attempted. Without [`ApplyOptions::permissions`] a dry run
//! therefore cannot know whether the caller may create or update, and says so by leaving
//! [`ApplySummary::permissions_checked`] `false`; do not tell a user "this will work" from such
//! a dry run.
//!
//! # The stream's fatal
//!
//! A whole-file failure (a torn file, a row past a limit) is the stream's last item
//! ([`ImportItem::Fatal`]). An all-or-nothing run that saw one writes nothing; a streaming run
//! stops and reports what it already committed, exactly as for any other write failure.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::document::Document;
use spot_app_api::error::AppError;
use spot_app_api::prelude::{RecordId, Value, Version};
use spot_meta::EffectiveMeta;

use crate::error::{CONFIG_BATCH_LABEL, MISSING_KEY_INDEX};
use crate::report::{ImportPolicy, ImportRowError};
use crate::stream::ImportItem;

mod write;

/// The most validated documents the default [`MemorySpool`] keeps.
pub const DEFAULT_SPOOL_CAPACITY: usize = 10_000;

/// How an import applies each validated parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyMode {
    /// Create every parent as a new draft.
    Insert,
    /// Patch the document the key field names; a key that names nothing is an error.
    Update {
        /// The field whose unique value names the document to patch.
        key_field: String,
    },
    /// Patch the document the key field names, or create it when the key names nothing.
    Upsert {
        /// The field whose unique value names the document to patch.
        key_field: String,
    },
    /// Write nothing; report what each parent would do.
    ///
    /// With no key field every parent would be created. With a key field the run plans an
    /// upsert: update when the key names a document whose values differ, create when it names
    /// nothing, skip when the stored values already match.
    DryRun {
        /// The field to plan an update by, when the dry run is of a keyed import.
        key_field: Option<String>,
    },
}

impl ApplyMode {
    /// The key field this mode looks documents up by, if any.
    pub fn key_field(&self) -> Option<&str> {
        match self {
            ApplyMode::Insert | ApplyMode::DryRun { key_field: None } => None,
            ApplyMode::Update { key_field } | ApplyMode::Upsert { key_field } => Some(key_field),
            ApplyMode::DryRun { key_field } => key_field.as_deref(),
        }
    }

    /// Whether this mode writes nothing.
    pub fn is_dry_run(&self) -> bool {
        matches!(self, ApplyMode::DryRun { .. })
    }
}

/// Finds the document an [`ApplyMode::Update`] or [`ApplyMode::Upsert`] patches.
///
/// The app API's [`DocumentService`] loads by record id and has no query method (queries need
/// the [`Repository`](spot_app_api::context::Repository) a host's internals hold), so the
/// caller supplies the index from whatever it used to find the keys — a query it already ran,
/// a cache, or the file's own key column when the import creates the documents. The writer
/// then loads the document through [`DocumentService::load`] to get the version it writes
/// against, so the index only has to answer with an id.
pub trait KeyIndex: Send + Sync {
    /// The id of the document of `doctype` whose `key_field` holds `value`, if any.
    fn find(&self, doctype: &str, key_field: &str, value: &Value) -> Option<RecordId>;
}

/// Answers whether the caller may create or update, before any write.
///
/// The app API's [`DocumentService`] has no permission probe: a refusal normally shows only
/// when the write is attempted. A host that can answer (its permission engine, under the same
/// [`Invocation`] the import runs with) supplies this so a dry run and the all-or-nothing
/// validation pass report refusals without writing. A refusal is reported as the row's error,
/// with the [`AppError`]'s own code and message.
pub trait PermissionProbe: Send + Sync {
    /// Whether `inv` may create a document of `doctype`.
    fn check_create(&self, inv: &Invocation, doctype: &str) -> Result<(), AppError>;

    /// Whether `inv` may update the document `id` of `doctype`.
    fn check_update(&self, inv: &Invocation, doctype: &str, id: RecordId) -> Result<(), AppError>;
}

/// Stops an import between documents.
///
/// A cheap, cloneable flag: the caller keeps one handle to cancel a long import, the writer
/// holds another. The check runs before each document, never inside one, so a document is
/// either wholly applied or not started.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the import to stop before its next document.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was asked for.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// One validated parent waiting in a [`Spool`].
#[derive(Clone, Debug)]
pub struct SpoolEntry {
    /// The 1-based source row the parent started at: its idempotency key's input.
    pub row: u64,
    /// The validated, defaulted document to write.
    pub document: Document,
}

/// The spool refused a parent: it is full. The parent was not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpoolFull;

/// Where an all-or-nothing import keeps validated parents until the whole stream has passed.
///
/// `push` adds at the back and `pop` takes from the front, so the write pass keeps file order.
/// Implementations are shared (`&self`), hence interior mutability: the writer only borrows the
/// options it was given. A disk- or file-backed implementation is a follow-up; the cap is part
/// of the contract so a huge file cannot exhaust memory silently.
pub trait Spool: Send + Sync {
    /// Stores one validated parent, or reports that the spool is full.
    fn push(&self, entry: SpoolEntry) -> Result<(), SpoolFull>;

    /// Takes the oldest stored parent.
    fn pop(&self) -> Option<SpoolEntry>;

    /// How many parents are stored.
    fn len(&self) -> usize;

    /// Whether the spool holds nothing.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most parents the spool stores before refusing.
    fn capacity(&self) -> usize;

    /// Drops everything (after a refused all-or-nothing run).
    fn clear(&self) {
        while self.pop().is_some() {}
    }
}

/// An in-memory [`Spool`] with a hard cap, for tests and for all-or-nothing files that fit.
#[derive(Debug)]
pub struct MemorySpool {
    entries: Mutex<VecDeque<SpoolEntry>>,
    capacity: usize,
}

impl MemorySpool {
    /// A spool holding at most `capacity` parents.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            capacity,
        }
    }
}

impl Default for MemorySpool {
    fn default() -> Self {
        Self::new(DEFAULT_SPOOL_CAPACITY)
    }
}

impl Spool for MemorySpool {
    fn push(&self, entry: SpoolEntry) -> Result<(), SpoolFull> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if entries.len() >= self.capacity {
            return Err(SpoolFull);
        }
        entries.push_back(entry);
        Ok(())
    }

    fn pop(&self) -> Option<SpoolEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
    }

    fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

/// The caller's view of how far an import has got.
///
/// Called once per [`ApplyOptions::progress_every`] documents and again when the run ends. Must
/// be quick: long work belongs to the caller.
pub trait ProgressSink {
    /// `done` documents have been seen; `total` is known only in the all-or-nothing write pass,
    /// where the validated parents are already counted.
    fn progress(&mut self, done: u64, total: Option<u64>);
}

/// A sink that ignores progress.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn progress(&mut self, _done: u64, _total: Option<u64>) {}
}

/// The options one import run uses.
///
/// Build it with [`ApplyOptions::new`], which fills the defaults (streaming `SkipBadRows`, no
/// posting, `max_errors` 100, the default in-memory spool); override the public fields for
/// everything else.
pub struct ApplyOptions {
    /// How each validated parent is applied.
    pub mode: ApplyMode,
    /// What to do with a file that has row problems.
    pub policy: ImportPolicy,
    /// Post every document this run creates, right after creating it.
    pub post_after_import: bool,
    /// Stop after this many failed documents; `0` means no limit. Default 100.
    pub max_errors: u64,
    /// The batch's identity: an input to every idempotency key (see the module docs). It must be
    /// unique per import (a job id, or a digest of the file). Empty by default: a real run
    /// refuses an empty label and the former default `"import"`.
    pub batch_label: String,
    /// Report progress every this many documents; `0` and `1` report every document.
    pub progress_every: u64,
    /// Stops the run between documents.
    pub cancel: CancelToken,
    /// The index the keyed modes find documents in; required by them.
    pub index: Option<Arc<dyn KeyIndex>>,
    /// Where an all-or-nothing run buffers validated parents.
    pub spool: Box<dyn Spool>,
    /// Answers create and update permission before any write; see [`PermissionProbe`].
    /// Required: there is no default and no `Option`.
    pub permissions: Arc<dyn PermissionProbe>,
    /// The target metadata, at the version the stream validated against: the writer builds the
    /// defaulted, validated [`Document`] out of each [`ImportDocument`] with it.
    pub meta: Arc<EffectiveMeta>,
}

impl ApplyOptions {
    /// The defaults of the brief, for an import of `meta`'s DocTypes. The permission probe is a
    /// required argument: there is no default and no `Option`.
    ///
    /// Building the options without a probe does not compile:
    ///
    /// ```compile_fail
    /// use std::sync::Arc;
    /// fn without_probe(meta: Arc<spot_meta::EffectiveMeta>) {
    ///     let _ = spot_import::ApplyOptions::new(meta);
    /// }
    /// ```
    ///
    /// and the same call with a probe does:
    ///
    /// ```
    /// use std::sync::Arc;
    /// use spot_app_api::context::Invocation;
    /// use spot_app_api::error::AppError;
    /// use spot_app_api::prelude::RecordId;
    /// use spot_import::PermissionProbe;
    ///
    /// struct Probe;
    /// impl PermissionProbe for Probe {
    ///     fn check_create(&self, _: &Invocation, _: &str) -> Result<(), AppError> { Ok(()) }
    ///     fn check_update(&self, _: &Invocation, _: &str, _: RecordId) -> Result<(), AppError> {
    ///         Ok(())
    ///     }
    /// }
    /// fn with_probe(meta: Arc<spot_meta::EffectiveMeta>) {
    ///     let _ = spot_import::ApplyOptions::new(meta, Arc::new(Probe));
    /// }
    /// ```
    pub fn new(meta: Arc<EffectiveMeta>, permissions: Arc<dyn PermissionProbe>) -> Self {
        Self {
            mode: ApplyMode::Insert,
            policy: ImportPolicy::SkipBadRows,
            post_after_import: false,
            max_errors: 100,
            batch_label: String::new(),
            progress_every: 100,
            cancel: CancelToken::new(),
            index: None,
            spool: Box::new(MemorySpool::new(DEFAULT_SPOOL_CAPACITY)),
            permissions,
            meta,
        }
    }

    /// Whether a real run lacks a usable batch label (empty, or the former default).
    fn bad_label(&self) -> bool {
        let label = self.batch_label.trim();
        !self.mode.is_dry_run() && (label.is_empty() || label == "import")
    }

    /// Whether a keyed mode is missing the index it cannot work without.
    fn missing_index(&self) -> bool {
        self.mode.key_field().is_some() && self.index.is_none()
    }
}

/// Why a run stopped before the end of its stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// [`ApplyOptions::max_errors`] failed documents were recorded.
    TooManyErrors,
    /// The caller cancelled through [`ApplyOptions::cancel`].
    Cancelled,
    /// A document could not be applied (a refused write, a key that disappeared); nothing
    /// further was attempted.
    WriteFailed,
    /// The all-or-nothing spool reached its cap; nothing was written.
    SpoolFull,
    /// The stream reported a whole-file failure ([`ImportItem::Fatal`]).
    StreamFailed,
    /// The all-or-nothing policy refused a file with row problems; nothing was written.
    PolicyRefused,
    /// The options were unusable, for example a keyed mode without its index.
    Configuration,
}

/// One draft this run created, as [`rollback_created`] needs it.
#[derive(Clone, Debug)]
pub struct CreatedDocument {
    /// The draft's DocType.
    pub doctype: String,
    /// The draft's record id.
    pub id: RecordId,
    /// The version the host returned: the draft's current one.
    pub version: Version,
    /// Whether [`ApplyOptions::post_after_import`] has already posted it (never deleted).
    pub posted: bool,
    /// The deterministic command id its create used, for the rollback key.
    pub command_id: String,
}

/// What a run did, in counts and in the report's own row-error format.
#[derive(Clone, Debug, Default)]
pub struct ApplySummary {
    /// Documents created (`0` in a keyed dry run; a draft whose post failed still counts).
    pub created: u64,
    /// Documents updated.
    pub updated: u64,
    /// Validated documents deliberately not written: the policy refused the file, a keyed
    /// update found nothing to change, or the run stopped before reaching them.
    pub skipped: u64,
    /// Documents that could not be imported at all (a bad row, a missing key, a refused write).
    pub failed: u64,
    /// Exactly the drafts this run created, in creation order.
    pub created_drafts: Vec<CreatedDocument>,
    /// Every problem, in the report's row format. A post-after-import failure is here even
    /// though its draft counts as created.
    pub errors: Vec<ImportRowError>,
    /// Whether the caller cancelled and the run stopped cleanly between documents.
    pub cancelled: bool,
    /// Why the run stopped early, when it did.
    pub stopped: Option<StopReason>,
    /// Whether this was a dry run (nothing was written; the counts are what would happen).
    pub dry_run: bool,
    /// Whether create/update permission was checked before writing (a [`PermissionProbe`] was
    /// supplied). `false` means a refusal can still surface at write time.
    pub permissions_checked: bool,
}

impl ApplySummary {
    /// Documents written (created and updated).
    pub fn written(&self) -> u64 {
        self.created + self.updated
    }
}

/// What [`apply_import`] did.
#[derive(Clone, Debug)]
pub enum ApplyOutcome {
    /// The run reached the end of its stream: everything the policy asked for is in place. A
    /// dry run writes nothing and is also `Applied`.
    Applied(ApplySummary),
    /// The run stopped after writing something. `failed_at` is the source row of the document
    /// that failed, or the row the run stopped at; roll back
    /// [`ApplySummary::created_drafts`].
    PartiallyApplied {
        /// What was written, and every problem so far.
        summary: ApplySummary,
        /// The source row the run stopped at (`0` when the stream as a whole failed).
        failed_at: u64,
    },
    /// Nothing was written: the policy refused the file, the spool was full, the caller
    /// cancelled before the first write, or the options were unusable.
    NothingWritten(ApplySummary),
}

impl ApplyOutcome {
    /// The counts and problems of any outcome.
    pub fn summary(&self) -> &ApplySummary {
        match self {
            ApplyOutcome::Applied(summary)
            | ApplyOutcome::PartiallyApplied { summary, .. }
            | ApplyOutcome::NothingWritten(summary) => summary,
        }
    }

    /// The counts and problems of any outcome, by value.
    pub fn into_summary(self) -> ApplySummary {
        match self {
            ApplyOutcome::Applied(summary)
            | ApplyOutcome::PartiallyApplied { summary, .. }
            | ApplyOutcome::NothingWritten(summary) => summary,
        }
    }
}

/// What [`rollback_created`] could and could not undo.
#[derive(Clone, Debug, Default)]
pub struct RollbackOutcome {
    /// Drafts deleted.
    pub deleted: u64,
    /// Drafts already posted: never deleted, left for the caller to cancel.
    pub needs_cancel: Vec<CreatedDocument>,
    /// Drafts the host refused to delete, with its error.
    pub failures: Vec<(CreatedDocument, AppError)>,
}

/// Applies a validated import stream to the host, one engine call per document.
///
/// See the module docs for idempotency, the all-or-nothing policy, dry runs and the memory
/// bounds. `items` is drained either as it is written ([`ImportPolicy::SkipBadRows`]) or fully
/// before the first write ([`ImportPolicy::AllOrNothing`]); `progress` hears how far the run
/// has got.
pub async fn apply_import(
    service: &dyn DocumentService,
    inv: &Invocation,
    items: impl Iterator<Item = ImportItem>,
    options: &ApplyOptions,
    progress: &mut dyn ProgressSink,
) -> ApplyOutcome {
    let mut summary = ApplySummary {
        dry_run: options.mode.is_dry_run(),
        permissions_checked: true,
        ..ApplySummary::default()
    };
    // A real run needs a label that identifies this import: it is half of every idempotency key.
    if options.bad_label() {
        summary.stopped = Some(StopReason::Configuration);
        summary.errors.push(write::note(
            0,
            CONFIG_BATCH_LABEL,
            "A real import needs a unique batch label (a job id or a digest of the file); an empty label or the old default \"import\" would share idempotency keys with other imports.",
        ));
        return ApplyOutcome::NothingWritten(summary);
    }
    // A keyed mode without its index cannot plan a single row: say so before touching anything.
    if options.missing_index() {
        summary.stopped = Some(StopReason::Configuration);
        summary.errors.push(write::note(
            0,
            MISSING_KEY_INDEX,
            "This import mode updates by a key field, so it needs a key index of the target DocType.",
        ));
        return ApplyOutcome::NothingWritten(summary);
    }
    if options.policy == ImportPolicy::AllOrNothing && !summary.dry_run {
        write::apply_refusing_writes(service, inv, items, options, progress, summary).await
    } else {
        write::apply_as_it_streams(service, inv, items, options, progress, summary).await
    }
}

/// Deletes the drafts a stopped run created, never a posted document.
///
/// The rollback runs in reverse creation order, so a document created after another is
/// removed before it. Each delete gets its own deterministic command id (the create's id plus
/// `-undo`), so a retried rollback is idempotent too. Posted documents are not deleted: they
/// come back in [`RollbackOutcome::needs_cancel`].
pub async fn rollback_created(
    service: &dyn DocumentService,
    inv: &Invocation,
    created: &[CreatedDocument],
) -> RollbackOutcome {
    let mut outcome = RollbackOutcome::default();
    for draft in created.iter().rev() {
        if draft.posted {
            outcome.needs_cancel.push(draft.clone());
            continue;
        }
        let undo = format!("{}-undo", draft.command_id);
        let call = match write::invocation_with_id(inv, &undo) {
            Ok(call) => call,
            Err(error) => {
                outcome.failures.push((draft.clone(), error));
                continue;
            }
        };
        match service
            .delete(&call, &draft.doctype, draft.id, draft.version)
            .await
        {
            Ok(_) => outcome.deleted += 1,
            Err(error) => outcome.failures.push((draft.clone(), error)),
        }
    }
    outcome
}

#[cfg(test)]
mod scale_tests;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_policy;
#[cfg(test)]
mod tests_safety;

/// Test doubles, available with the `testing` feature (and to this crate's own tests).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use spot_app_api::context::Invocation;
    use spot_app_api::error::AppError;

    /// A permission probe that allows everything.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct AllowAll;

    impl PermissionProbe for AllowAll {
        fn check_create(&self, _inv: &Invocation, _doctype: &str) -> Result<(), AppError> {
            Ok(())
        }
        fn check_update(
            &self,
            _inv: &Invocation,
            _doctype: &str,
            _id: spot_app_api::prelude::RecordId,
        ) -> Result<(), AppError> {
            Ok(())
        }
    }

    /// A permission probe that denies everything.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DenyAll;

    impl PermissionProbe for DenyAll {
        fn check_create(&self, _inv: &Invocation, doctype: &str) -> Result<(), AppError> {
            Err(AppError::new(
                spot_app_api::error::ErrorClass::PermissionDenied,
                "permission.denied",
                format!("you may not create {doctype}"),
            ))
        }
        fn check_update(
            &self,
            _inv: &Invocation,
            doctype: &str,
            _id: spot_app_api::prelude::RecordId,
        ) -> Result<(), AppError> {
            Err(AppError::new(
                spot_app_api::error::ErrorClass::PermissionDenied,
                "permission.denied",
                format!("you may not update {doctype}"),
            ))
        }
    }
}
