//! The working state of one storage transaction, shared by the operation and everything it
//! triggers.

use std::fmt;

use spot_app_api::Document;
use spot_app_api::error::{StorageOp, map_storage_error};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{AppError, AppResult, HookPoint, Message, Operation};
use spot_storage_contract::{ErrorKind, OutboxEvent, StorageError};

/// Secret columns already sealed, waiting for the write stage of their record: the sealed bytes
/// (and blind indexes) of `create_with_secrets` and `set_secret`. Never plaintext.
#[derive(Clone)]
pub(crate) struct SealedWrites {
    /// The secret fields written, for the lifecycle event (names only).
    pub(crate) fields: Vec<String>,
    /// `(column, Value::Bytes)` pairs: `<field>__sealed` and `<field>__bidx`.
    pub(crate) columns: Vec<(Name, Value)>,
    /// The audit events of the secrets written, enqueued in the same transaction once the write
    /// stage succeeded (and so rolled back with it). They name fields, never values.
    pub(crate) audit: Vec<OutboxEvent>,
}

impl fmt::Debug for SealedWrites {
    /// Names the fields; the bytes (even sealed ones) never reach a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedWrites")
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

/// The sealed columns the write of one record must carry.
pub(crate) struct PendingSecrets {
    pub(crate) doctype: String,
    pub(crate) id: RecordId,
    pub(crate) writes: SealedWrites,
}

/// A committed step waiting for its transaction to commit.
pub(crate) struct CommittedItem {
    /// The document as the step sees it: as written, or as it was before a deletion.
    pub(crate) doc: Document,
    pub(crate) point: HookPoint,
    /// The operation whose committed phase this is (`save_committed` runs for both create
    /// and update, so the point alone does not say).
    pub(crate) operation: Operation,
}

use super::capture::CaptureSet;
use super::participant::Cause;
use super::receipt::{Finished, ReceiptCollector};

/// How much of the working state existed at a savepoint.
pub(crate) struct Mark {
    messages: usize,
    committed: usize,
    finished: usize,
    /// The capture set as it was: a rolled-back write is forgotten with the rest of its work.
    capture: CaptureSet,
}

/// One open transaction and what the operations running in it have collected.
pub(crate) struct Work<T> {
    pub(crate) tx: T,
    /// Messages of every step, in order.
    pub(crate) messages: Vec<Message>,
    /// Committed steps to run after the commit, in order.
    pub(crate) committed: Vec<CommittedItem>,
    /// The first storage error after which the transaction cannot go on, and what the engine
    /// was doing. When set, the operation fails with it however the steps answered.
    pub(crate) poisoned: Option<(StorageError, StorageOp)>,
    /// Whether anything was deleted: a link violation at commit then means "linked".
    pub(crate) deleted: bool,
    /// The first document this transaction deleted, for mapping a reference violation that
    /// storage reports only at commit.
    pub(crate) deleted_target: Option<(String, RecordId)>,
    /// How deeply writes from steps are nested.
    pub(crate) depth: u32,
    /// For a `rename`: the title the document had before the rename, for the event's
    /// `old_name`. The repository offers no nested rename, so one value is enough.
    pub(crate) rename_from: Option<String>,
    /// For `set_secret` and `create_with_secrets`: the sealed columns the write stage of that
    /// one record merges into its row. A write of any other record never sees them.
    pub(crate) pending_secrets: Option<PendingSecrets>,
    /// One collector per operation running in the transaction, innermost last: the steps of an
    /// operation report their effects into its own (WP-DOC-12.T1).
    pub(crate) collectors: Vec<ReceiptCollector>,
    /// The receipts of the operations that finished, in the order they finished.
    pub(crate) finished: Vec<Finished>,
    /// Why the write now running is made (WP-DOC-04.T5).
    pub(crate) cause: Cause,
    /// For `correct`: the name the amended copy takes (`<base>-<n>`), chosen in this
    /// transaction by [`Engine::amendment_name`](super::naming) and consumed by the copy's own
    /// naming at its create. Only an amendment of a `field:` or `prompt` DocType has one.
    pub(crate) amended_name: Option<String>,
    /// The name of the whitelisted method running, while its body runs.
    pub(crate) method: Option<String>,
    /// What a refusal needs to be reported with, after the rollback.
    pub(crate) trail: Trail,
    /// The documents written, for the `doc_state` events flushed before the commit.
    pub(crate) capture: CaptureSet,
}

/// What the denial sink is told that the error alone does not say.
#[derive(Clone, Debug, Default)]
pub(crate) struct Trail {
    /// Why the write that was running when the operation failed was made.
    pub(crate) cause: Cause,
    /// The refusal behind a `method.not_found` the caller was given to hide it.
    pub(crate) masked: Option<AppError>,
    /// The last error a participant returned: a refusal when it is the operation's error.
    pub(crate) participant: Option<AppError>,
}

/// The storage error kinds after which a transaction may go on, provided the failed operation
/// ran inside a savepoint that was rolled back (storage contract, `Transaction`).
fn recoverable(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::NotFound
            | ErrorKind::StaleVersion
            | ErrorKind::AlreadyExists
            | ErrorKind::UniqueViolation
            | ErrorKind::InvalidValue
            | ErrorKind::InvalidQuery
            | ErrorKind::UnknownEntity
            | ErrorKind::UnknownField
            | ErrorKind::LockNotAvailable
    )
}

impl<T> Work<T> {
    pub(crate) fn new(tx: T) -> Self {
        Self {
            tx,
            messages: Vec::new(),
            committed: Vec::new(),
            poisoned: None,
            deleted: false,
            deleted_target: None,
            depth: 0,
            rename_from: None,
            pending_secrets: None,
            collectors: Vec::new(),
            finished: Vec::new(),
            cause: Cause::Direct,
            amended_name: None,
            method: None,
            trail: Trail::default(),
            capture: CaptureSet::default(),
        }
    }

    /// Maps a storage error for the caller, and poisons the transaction when it cannot go on.
    pub(crate) fn fail(&mut self, e: StorageError, during: StorageOp) -> AppError {
        if !recoverable(e.kind) {
            self.poison(e.clone(), during);
        }
        map_storage_error(&e, during)
    }

    /// Remembers an error a participant returned, so a refusal by one can be reported.
    pub(crate) fn note_participant(&mut self, result: AppResult<()>) -> AppResult<()> {
        if let Err(error) = &result {
            self.trail.participant = Some(error.clone());
        }
        result
    }

    /// Marks the transaction as unusable (keeps the first cause).
    pub(crate) fn poison(&mut self, e: StorageError, during: StorageOp) {
        if self.poisoned.is_none() {
            self.poisoned = Some((e, during));
        }
    }

    pub(crate) fn mark(&self) -> Mark {
        Mark {
            messages: self.messages.len(),
            committed: self.committed.len(),
            finished: self.finished.len(),
            capture: self.capture.clone(),
        }
    }

    /// Forgets what was collected after `mark` (the work was rolled back to a savepoint).
    pub(crate) fn truncate(&mut self, mark: &Mark) {
        self.messages.truncate(mark.messages);
        self.committed.truncate(mark.committed);
        self.finished.truncate(mark.finished);
        self.capture = mark.capture.clone();
    }

    /// Hands what a hook point's steps reported to the operation they ran for.
    pub(crate) fn collect(&mut self, reports: ReceiptCollector) {
        if let Some(collector) = self.collectors.last_mut() {
            collector.absorb(reports);
        }
    }
}
