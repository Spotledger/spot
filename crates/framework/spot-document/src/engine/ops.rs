//! Top-level operations: the transaction, its retries, the ADR-51 order and the write stage.

use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value as Json, json};
use spot_app_api::context::{AMENDED_FROM, Invocation, Outcome};
use spot_app_api::error::{StorageOp, map_storage_error};
use spot_app_api::lifecycle::{Phase, Stage, op_name, sequence};
use spot_app_api::prelude::{Name, RecordId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, ChildRow, DocState, DocTypeDef, Document, DocumentPatch,
    ErrorClass, FieldPath, HookPoint, Message, Operation, Target,
};
use spot_permissions::Action;
use spot_storage_contract::{DocumentStore, Outbox, OutboxEvent, QueryExecutor};
use spot_storage_contract::{
    ErrorKind, Isolation, Query, RowLock, Rows, StorageBackend, StorageError, Transaction,
    TxOptions, Wait,
};
use tracing::{Instrument, Span};

use super::authz::{WriteChange, create_changes, patch_changes};
use super::committed::{CommittedBatch, CommittedStep, CommittedSteps, OutboxSink, StepOutcome};
use super::ctx::Ctx;
use super::links;
use super::naming;
use super::participant::{
    Cause, Denial, NodeId, Participants, WriteIntent, WriteRecord, report_denial, run_after,
    run_before,
};
use super::receipt::{Announced, Finished, ReceiptCollector, merge_announced};
use super::work::{CommittedItem, PendingSecrets, SealedWrites, Trail, Work};
use super::{CommittedFailure, Engine, Snapshot, initial_state, outbox};
use crate::lookup::name;
use crate::lookup::resolve;
use crate::mapping::{from_storage, secret_columns, secret_fields, to_storage};
use crate::{coerce, defaults, validate};

/// The most levels of writes from steps one transaction runs (a step that writes a document
/// whose step writes a document ...). Deeper than any real chain; it stops runaway recursion.
const MAX_DEPTH: u32 = 32;

/// A top-level request, owned so a retry can run it again.
#[derive(Clone)]
pub(crate) enum Req {
    Create(Document),
    Update {
        doctype: String,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    },
    Delete {
        doctype: String,
        id: RecordId,
        expected: Version,
    },
    /// Post a draft (ADR-42).
    Post {
        doctype: String,
        id: RecordId,
        expected: Version,
    },
    /// Cancel a posted document (ADR-42).
    Cancel {
        doctype: String,
        id: RecordId,
        expected: Version,
    },
    /// Cancel a posted document and create its amended draft in one transaction (ADR-42).
    Correct {
        doctype: String,
        id: RecordId,
        expected: Version,
    },
    /// Change the title field (the human-readable name); the id never changes.
    Rename {
        doctype: String,
        id: RecordId,
        new_name: String,
        expected: Version,
    },
    /// Call a whitelisted method on a document (ADR-16).
    Method {
        doctype: String,
        id: RecordId,
        method: String,
        args: Json,
        expected: Option<Version>,
    },
    /// Write sealed secret columns of an existing record (`set_secret`): the record's update
    /// lifecycle with no field changed, so the version, the hooks and the event are the normal
    /// ones.
    SetSecrets {
        doctype: String,
        id: RecordId,
        writes: SealedWrites,
        expected: Version,
    },
    /// Create a record together with its sealed secret columns (`create_with_secrets`).
    CreateWithSecrets {
        doc: Document,
        writes: SealedWrites,
    },
}

impl Req {
    fn doctype(&self) -> &str {
        match self {
            Req::Create(doc) | Req::CreateWithSecrets { doc, .. } => doc.doctype().as_str(),
            Req::Update { doctype, .. }
            | Req::SetSecrets { doctype, .. }
            | Req::Delete { doctype, .. }
            | Req::Post { doctype, .. }
            | Req::Cancel { doctype, .. }
            | Req::Correct { doctype, .. }
            | Req::Rename { doctype, .. }
            | Req::Method { doctype, .. } => doctype,
        }
    }

    /// The id of the document the request is about.
    fn target_id(&self) -> Option<RecordId> {
        match self {
            Req::Create(doc) | Req::CreateWithSecrets { doc, .. } => Some(doc.id()),
            Req::Update { id, .. }
            | Req::SetSecrets { id, .. }
            | Req::Delete { id, .. }
            | Req::Post { id, .. }
            | Req::Cancel { id, .. }
            | Req::Correct { id, .. }
            | Req::Rename { id, .. }
            | Req::Method { id, .. } => Some(*id),
        }
    }

    /// Whether the result is returned as the engine wrote it, without the principal's field
    /// filter: the secret operations are made with a [`SecretAccess`](crate::secrets) and have
    /// no principal to filter for.
    fn unmasked(&self) -> bool {
        matches!(self, Req::SetSecrets { .. } | Req::CreateWithSecrets { .. })
    }

    fn op(&self) -> &'static str {
        match self {
            Req::Create(_) | Req::CreateWithSecrets { .. } => "create",
            Req::SetSecrets { .. } => "update",
            Req::Update { .. } => "update",
            Req::Delete { .. } => "delete",
            Req::Post { .. } => "post",
            Req::Cancel { .. } => "cancel",
            Req::Correct { .. } => "correct",
            Req::Rename { .. } => "rename",
            Req::Method { .. } => "method",
        }
    }
}

/// What an operation returns.
pub(crate) enum Ret {
    Doc(Document),
    Unit,
    DocJson(Document, Json),
}

/// A finished top-level operation: its result and the messages of every step.
pub(crate) struct Done {
    pub(crate) value: Ret,
    pub(crate) messages: Vec<Message>,
    /// The receipts of the operations the request ran (WP-DOC-12.T1).
    pub(crate) receipts: Vec<Finished>,
}

impl Done {
    pub(crate) fn into_doc(self) -> AppResult<Outcome<Document>> {
        match self.value {
            Ret::Doc(value) => Ok(Outcome {
                value,
                messages: self.messages,
            }),
            Ret::Unit | Ret::DocJson(..) => Err(internal("the operation returned no document")),
        }
    }

    pub(crate) fn into_unit(self) -> AppResult<Outcome<()>> {
        Ok(Outcome {
            value: (),
            messages: self.messages,
        })
    }

    pub(crate) fn into_doc_json(self) -> AppResult<Outcome<(Document, Json)>> {
        match self.value {
            Ret::DocJson(value, result) => Ok(Outcome {
                value: (value, result),
                messages: self.messages,
            }),
            Ret::Doc(_) | Ret::Unit => Err(internal("the method returned no result")),
        }
    }
}

impl Ret {
    pub(crate) fn doc(self) -> AppResult<Document> {
        match self {
            Ret::Doc(doc) | Ret::DocJson(doc, _) => Ok(doc),
            Ret::Unit => Err(internal("the operation returned no document")),
        }
    }
}

/// The `doc.phase` span for `name`: the open one if the previous stage was in the same phase,
/// else a new one (the previous one ends when its last handle drops).
fn phase_in(slot: &mut Option<(&'static str, Span)>, name: &'static str) -> Span {
    match slot {
        Some((open, span)) if *open == name => span.clone(),
        _ => {
            let span = tracing::info_span!("doc.phase", phase = name);
            *slot = Some((name, span.clone()));
            span
        }
    }
}

/// How an operation ended, as a span field: `ok`, `stale` (the version moved), `refused` (a
/// step, a check or a permission said no) or `failed` (infrastructure or a programming error).
fn outcome_of<T>(result: &AppResult<T>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(e) => match e.class {
            ErrorClass::StaleVersion => "stale",
            ErrorClass::Internal | ErrorClass::Unavailable => "failed",
            _ => "refused",
        },
    }
}

fn internal(msg: &str) -> AppError {
    AppError::new(ErrorClass::Internal, "document.internal_error", msg)
}

/// Why one attempt of an operation failed.
pub(crate) enum Failed {
    /// A step or a check refused, or the operation is not allowed.
    App(AppError),
    /// Storage failed in a way that ends the transaction.
    Storage(StorageError, StorageOp),
    /// A step's nested write met a newer version of another document and the error left the
    /// operation (WP-DOC-04.T7). The document the operation was asked to change was checked
    /// under its own row lock, so the whole operation may run again and read the fresh one.
    Conflict(AppError),
}

impl Failed {
    /// The storage error kind to retry on, if this failure may be retried.
    fn retry_cause(&self) -> Option<ErrorKind> {
        match self {
            Failed::Storage(e, _) if e.kind.is_retryable() => Some(e.kind),
            Failed::Conflict(_) => Some(ErrorKind::StaleVersion),
            _ => None,
        }
    }

    fn into_app(self) -> AppError {
        match self {
            Failed::App(e) => e,
            Failed::Storage(e, during) => map_storage_error(&e, during),
            Failed::Conflict(e) => conflict(&e),
        }
    }
}

impl From<AppError> for Failed {
    fn from(e: AppError) -> Self {
        Failed::App(e)
    }
}

pub(crate) fn not_found() -> AppError {
    AppError::new(
        ErrorClass::NotFound,
        "document.not_found",
        "The document does not exist.",
    )
}

fn state_error(what: &str) -> AppError {
    AppError::new(
        ErrorClass::InvalidState,
        "document.invalid_state",
        format!("{what} is not allowed in the document's state."),
    )
}

fn stale(current: Option<Version>) -> AppError {
    AppError::new(
        ErrorClass::StaleVersion,
        "document.stale_version",
        "The document was changed by someone else.",
    )
    .detail("current_version", current.map_or(0, |v| v.0))
}

/// The detail keys that mark a stale version met by a step's nested write: the document the
/// nested write was about. They are set by [`Engine::nested`] and read by the attempt; they
/// never leave the engine (the error that does is [`conflict`]).
const STEP_DOCTYPE: &str = "step_doctype";
const STEP_ID: &str = "step_id";

/// Marks a stale version raised by a step's nested write with the document it was about, unless
/// a deeper write already did.
fn tag_step_stale(mut error: AppError, doctype: &str, id: Option<RecordId>) -> AppError {
    if error.code == "document.stale_version" && !error.details.contains_key(STEP_DOCTYPE) {
        error
            .details
            .insert(STEP_DOCTYPE.to_owned(), Json::from(doctype));
        if let Some(id) = id {
            error
                .details
                .insert(STEP_ID.to_owned(), Json::from(id.to_string()));
        }
    }
    error
}

/// Whether `error` is a stale version a step's nested write met (see [`tag_step_stale`]).
fn is_step_stale(error: &AppError) -> bool {
    error.code == "document.stale_version" && error.details.contains_key(STEP_DOCTYPE)
}

/// The answer when the reruns of an operation ran out while a step's nested write kept meeting
/// a newer version: a retryable conflict that names the document, never a version of it.
fn conflict(tagged: &AppError) -> AppError {
    let mut error = AppError::new(
        ErrorClass::Unavailable,
        "document.conflict",
        "A document the operation changes was changed by someone else; try again.",
    );
    for key in [STEP_DOCTYPE, STEP_ID] {
        if let Some(value) = tagged.details.get(key) {
            let name = if key == STEP_DOCTYPE { "doctype" } else { "id" };
            error = error.detail(name, value.clone());
        }
    }
    error
}

/// The transaction label `<doctype>_<op>`. Storage names have no dots, so the dot of the
/// documented form is an underscore; a name too long for storage falls back to the operation.
pub(crate) fn label(doctype: &str, op: &str) -> Name {
    Name::new(&format!("{doctype}_{op}"))
        .or_else(|_| Name::new(op))
        .unwrap_or_else(|_| unreachable_label())
}

fn unreachable_label() -> Name {
    // `op` is one of the fixed words above, all valid storage names.
    match Name::new("operation") {
        Ok(n) => n,
        Err(e) => unreachable!("`operation` is a valid name: {e}"),
    }
}

/// Writes the audit events of a secret write to the transaction's outbox. A failure fails the
/// operation: no secret is set without its audit row.
async fn enqueue_audit<T: spot_storage_contract::Transaction>(
    work: &mut Work<T>,
    events: &[OutboxEvent],
) -> AppResult<()> {
    for event in events {
        work.tx
            .enqueue(event)
            .await
            .map_err(|e| work.fail(e, StorageOp::Write))?;
    }
    Ok(())
}

pub(crate) fn title(def: &DocTypeDef, doc: &Document) -> Json {
    def.title_field
        .as_ref()
        .and_then(|f| match doc.get(f.as_str()) {
            Value::Text(t) => Some(Json::String(t.clone())),
            _ => None,
        })
        .unwrap_or(Json::Null)
}

/// The title field's text, for a rename's `old_name`; empty when unset or not text.
fn title_text(def: &DocTypeDef, doc: &Document) -> String {
    def.title_field
        .as_ref()
        .and_then(|f| match doc.get(f.as_str()) {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

impl<B: StorageBackend> Engine<B> {
    /// Runs the whole request again while it fails with a retryable storage error, waiting
    /// longer each time, at most [`RetryPolicy::max_retries`](super::RetryPolicy) times.
    /// `CommitOutcomeUnknown` is not retryable.
    pub(crate) async fn retrying<T, F, Fut>(
        &self,
        label: &Name,
        mut attempt_once: F,
    ) -> AppResult<T>
    where
        F: FnMut() -> Fut + Send,
        Fut: Future<Output = Result<T, Failed>> + Send,
    {
        let mut attempt = 0u32;
        loop {
            match attempt_once().await {
                Ok(value) => return Ok(value),
                Err(failed) => match failed.retry_cause() {
                    Some(cause) if attempt < self.retry.max_retries => {
                        attempt += 1;
                        self.record_retry(label.as_str(), attempt, cause);
                        let wait = self.retry.delay(attempt);
                        if !wait.is_zero() {
                            tokio::time::sleep(wait).await;
                        }
                    }
                    _ => return Err(failed.into_app()),
                },
            }
        }
    }

    /// One top-level operation: a transaction (retried as a whole), then the committed steps.
    ///
    /// The whole operation is one `doc.op` span (WP-PLT-15.T3): the DocType and operation names,
    /// the number of the attempt now running, and at the end the retries and the outcome. No
    /// document value, id or error text is ever a field.
    pub(crate) fn top<'a>(
        &'a self,
        inv: &'a Invocation,
        req: Req,
    ) -> BoxFuture<'a, AppResult<Done>> {
        let span = tracing::info_span!(
            "doc.op",
            doctype = req.doctype(),
            operation = req.op(),
            attempt = tracing::field::Empty,
            retries = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        Box::pin(
            async move {
                let attempts = AtomicU32::new(0);
                let result = self.top_inner(inv, req, &attempts).await;
                let span = Span::current();
                span.record(
                    "retries",
                    attempts.load(Ordering::Relaxed).saturating_sub(1),
                );
                span.record("outcome", outcome_of(&result));
                result
            }
            .instrument(span),
        )
    }

    async fn top_inner(&self, inv: &Invocation, req: Req, attempts: &AtomicU32) -> AppResult<Done> {
        {
            let snap = self.snapshot();
            let label = label(req.doctype(), req.op());
            let unmasked = req.unmasked();
            let trail = Mutex::new(Trail::default());
            let attempted = self
                .retrying(&label, || {
                    let number = attempts.fetch_add(1, Ordering::Relaxed) + 1;
                    Span::current().record("attempt", number);
                    self.attempt(&snap, inv, &req, &label, &trail)
                })
                .await;
            let (value, mut messages, committed, mut receipts) = match attempted {
                Ok(done) => done,
                Err(error) => {
                    // The transaction has rolled back by now: tell the sink, never inside it.
                    let trail = trail.into_inner().unwrap_or_else(|p| p.into_inner());
                    self.report_refusal(inv, &req, &trail, &error).await;
                    return Err(error);
                }
            };
            // A write result is a read of the document as written: the same field filter a load
            // gets (WP-PLT-03.T2).
            let value = if unmasked {
                value
            } else {
                let principal = self.acting_principal(inv).await;
                self.mask_result(&principal, value).await?
            };
            let batch = batch_of(inv, &committed);
            let steps = EngineSteps {
                engine: self,
                snap: &snap,
                announced: Mutex::new(Vec::new()),
            };
            // Phase 4 is one span: the delivery path runs the steps (each a `doc.step`) in it.
            let delivered = self
                .delivery
                .deliver(&steps, batch)
                .instrument(tracing::info_span!("doc.phase", phase = "committed"))
                .await;
            messages.extend(delivered);
            // The external effects announced while delivering belong in the receipts returned.
            let announced = steps
                .announced
                .into_inner()
                .unwrap_or_else(|p| p.into_inner());
            merge_announced(&mut receipts, announced);
            Ok(Done {
                value,
                messages,
                receipts,
            })
        }
    }

    /// Tells the denial sink about a refused operation, if one is installed and the error is a
    /// refusal (WP-DOC-04.T5): permissions, a version conflict, validation, or a participant.
    /// Not a missing document, a transient storage error or an internal fault. The sink gets
    /// the error's class and code only.
    async fn report_refusal(&self, inv: &Invocation, req: &Req, trail: &Trail, error: &AppError) {
        let Some(sink) = self.denial_sink() else {
            return;
        };
        // A method call the caller was told is `method.not_found` to hide what it may not see
        // is still a refusal of a right.
        let real = match (&trail.masked, error.class) {
            (Some(masked), ErrorClass::MethodNotFound) => masked,
            _ => error,
        };
        let refusal = match real.class {
            ErrorClass::PermissionDenied | ErrorClass::StaleVersion | ErrorClass::Validation => {
                true
            }
            ErrorClass::NotFound
            | ErrorClass::Unavailable
            | ErrorClass::Internal
            | ErrorClass::MethodNotFound => false,
            _ => trail.participant.as_ref() == Some(real),
        };
        if !refusal {
            return;
        }
        let bare = AppError::new(real.class, &real.code, String::new());
        let (operation, id, cause) = match req {
            Req::Create(doc) | Req::CreateWithSecrets { doc, .. } => {
                (Operation::Create, Some(doc.id()), Cause::Direct)
            }
            Req::Update { id, .. } | Req::SetSecrets { id, .. } => {
                (Operation::Update, Some(*id), Cause::Direct)
            }
            Req::Delete { id, .. } => (Operation::Delete, Some(*id), Cause::Direct),
            Req::Post { id, .. } => (Operation::Post, Some(*id), Cause::Direct),
            Req::Cancel { id, .. } => (Operation::Cancel, Some(*id), Cause::Direct),
            Req::Rename { id, .. } => (Operation::Rename, Some(*id), Cause::Direct),
            Req::Method { id, method, .. } => {
                (Operation::Update, Some(*id), Cause::Method(method.clone()))
            }
            Req::Correct { id, .. } => match trail.cause {
                Cause::CorrectAmend => (Operation::Create, Some(*id), Cause::CorrectAmend),
                _ => (Operation::Cancel, Some(*id), Cause::CorrectCancel),
            },
        };
        let denial = Denial {
            doctype: req.doctype(),
            id,
            operation,
            cause: cause.view(),
            invocation: inv,
            error: &bare,
        };
        report_denial(sink.as_ref(), &denial, self.denial_timeout).await;
    }

    /// One attempt: open the transaction, run the request, commit.
    async fn attempt(
        &self,
        snap: &Snapshot,
        inv: &Invocation,
        req: &Req,
        label: &Name,
        trail: &Mutex<Trail>,
    ) -> Result<(Ret, Vec<Message>, Vec<CommittedItem>, Vec<Finished>), Failed> {
        let options = TxOptions::new(label.clone(), Isolation::ReadCommitted);
        let tx = self
            .backend
            .begin(snap.catalog.clone(), options)
            .await
            .map_err(|e| Failed::Storage(e, StorageOp::Other))?;
        let mut work = Work::new(tx);
        let result = self.exec(snap, &mut work, inv, req.clone()).await;
        let outcome = match (result, work.poisoned.take()) {
            (Ok(_), Some((e, during))) => Err(Failed::Storage(e, during)),
            (Err(_), Some((e, during))) if e.kind.is_retryable() => Err(Failed::Storage(e, during)),
            (Err(e), _) if is_step_stale(&e) => Err(Failed::Conflict(e)),
            (Err(e), _) => Err(Failed::App(e)),
            (Ok(value), None) => Ok(value),
        };
        match outcome {
            Err(failed) => {
                work.trail.cause = work.cause.clone();
                *trail.lock().unwrap_or_else(|p| p.into_inner()) = std::mem::take(&mut work.trail);
                // Nothing to add to the error if the rollback fails too: the transaction is gone.
                let _ = work.tx.rollback().await;
                Err(failed)
            }
            Ok(value) => {
                // The durable delivery writes its job in this transaction, after the last write.
                // Only the points some step is planned at: a write nobody listens to costs no job.
                let planned: Vec<CommittedItem> = work
                    .committed
                    .iter()
                    .filter(|c| {
                        !snap
                            .seams
                            .plan(c.doc.doctype().as_str(), &c.point)
                            .is_empty()
                    })
                    .map(|c| CommittedItem {
                        doc: c.doc.clone(),
                        point: c.point.clone(),
                        operation: c.operation,
                    })
                    .collect();
                if !planned.is_empty() {
                    let batch = batch_of(inv, &planned);
                    let mut sink = TxOutbox {
                        tx: &mut work.tx,
                        failed: None,
                    };
                    let prepared = self.delivery.prepare(&mut sink, &batch).await;
                    let storage = sink.failed.take();
                    if let Err(error) = prepared {
                        let _ = work.tx.rollback().await;
                        return Err(match storage {
                            Some(e) => Failed::Storage(e, StorageOp::Write),
                            None => Failed::App(error),
                        });
                    }
                }
                // The journal's state events: one per document written, in this transaction.
                if let Err(error) = self.flush_capture(snap, &mut work, inv).await {
                    let failed = match work.poisoned.take() {
                        Some((e, during)) => Failed::Storage(e, during),
                        None => Failed::App(error),
                    };
                    let _ = work.tx.rollback().await;
                    return Err(failed);
                }
                let Work {
                    tx,
                    messages,
                    committed,
                    deleted,
                    deleted_target,
                    finished,
                    ..
                } = work;
                let during = if deleted {
                    StorageOp::Delete
                } else {
                    StorageOp::Write
                };
                tx.commit().await.map_err(|e| {
                    match links::commit_violation(snap, deleted_target.as_ref(), &e) {
                        Some(linked) => Failed::App(linked),
                        None => Failed::Storage(e, during),
                    }
                })?;
                Ok((value, messages, committed, finished))
            }
        }
    }

    /// Executes a request inside the open transaction `work`.
    pub(crate) fn exec<'a>(
        &'a self,
        snap: &'a Snapshot,
        work: &'a mut Work<B::Tx>,
        inv: &'a Invocation,
        req: Req,
    ) -> BoxFuture<'a, AppResult<Ret>> {
        Box::pin(async move {
            match req {
                Req::Create(doc) => self.do_create(snap, work, inv, doc).await.map(Ret::Doc),
                Req::Update {
                    doctype,
                    id,
                    patch,
                    expected,
                } => self
                    .do_update(snap, work, inv, &doctype, id, patch, expected)
                    .await
                    .map(Ret::Doc),
                Req::Delete {
                    doctype,
                    id,
                    expected,
                } => self
                    .do_delete(snap, work, inv, &doctype, id, expected)
                    .await
                    .map(|_| Ret::Unit),
                Req::Post {
                    doctype,
                    id,
                    expected,
                } => self
                    .do_transition(snap, work, inv, Operation::Post, &doctype, id, expected)
                    .await
                    .map(Ret::Doc),
                Req::Cancel {
                    doctype,
                    id,
                    expected,
                } => self
                    .do_transition(snap, work, inv, Operation::Cancel, &doctype, id, expected)
                    .await
                    .map(Ret::Doc),
                Req::Correct {
                    doctype,
                    id,
                    expected,
                } => self
                    .do_correct(snap, work, inv, &doctype, id, expected)
                    .await
                    .map(Ret::Doc),
                Req::Rename {
                    doctype,
                    id,
                    new_name,
                    expected,
                } => self
                    .do_rename(snap, work, inv, &doctype, id, &new_name, expected)
                    .await
                    .map(Ret::Doc),
                Req::Method {
                    doctype,
                    id,
                    method,
                    args,
                    expected,
                } => self
                    .do_method(snap, work, inv, &doctype, id, &method, args, expected)
                    .await
                    .map(|(doc, result)| Ret::DocJson(doc, result)),
                Req::SetSecrets {
                    doctype,
                    id,
                    writes,
                    expected,
                } => self
                    .do_set_secrets(snap, work, inv, &doctype, id, writes, expected)
                    .await
                    .map(Ret::Doc),
                Req::CreateWithSecrets { doc, writes } => self
                    .do_create_with_secrets(snap, work, inv, doc, writes)
                    .await
                    .map(Ret::Doc),
            }
        })
    }

    /// A write made by a step in phase 3: the full lifecycle of the target, in the same
    /// transaction, inside a savepoint, so that an error the step handles leaves nothing behind.
    pub(crate) fn nested<'a>(
        &'a self,
        snap: &'a Snapshot,
        work: &'a mut Work<B::Tx>,
        inv: &'a Invocation,
        req: Req,
    ) -> BoxFuture<'a, AppResult<Ret>> {
        Box::pin(async move {
            if work.depth >= MAX_DEPTH {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "document.internal_error",
                    format!("Steps wrote documents more than {MAX_DEPTH} levels deep."),
                ));
            }
            let savepoint = work
                .tx
                .savepoint()
                .await
                .map_err(|e| work.fail(e, StorageOp::Other))?;
            let mark = work.mark();
            // A write the whitelisted method's own body makes; any deeper write is a step's.
            let cause = match (&work.method, work.depth) {
                (Some(name), 0) => Cause::Method(name.clone()),
                _ => Cause::Step,
            };
            let step =
                matches!(cause, Cause::Step).then(|| (req.doctype().to_owned(), req.target_id()));
            let outer = std::mem::replace(&mut work.cause, cause);
            work.depth += 1;
            let result = self.exec(snap, work, inv, req).await;
            work.depth -= 1;
            work.cause = outer;
            match result {
                Ok(value) => {
                    work.tx
                        .release(savepoint)
                        .await
                        .map_err(|e| work.fail(e, StorageOp::Other))?;
                    Ok(value)
                }
                Err(error) => {
                    if let Err(e) = work.tx.rollback_to(savepoint).await {
                        work.poison(e, StorageOp::Other);
                    }
                    work.truncate(&mark);
                    // A step's write met a newer version: tag it, so that if it leaves the
                    // operation the operation reruns. A method body's own write is not tagged.
                    Err(match step {
                        Some((doctype, id)) => tag_step_stale(error, &doctype, id),
                        None => error,
                    })
                }
            }
        })
    }

    async fn do_create(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doc: Document,
    ) -> AppResult<Document> {
        let def = snap.doctype(doc.doctype().as_str())?;
        if def.is_child {
            return Err(AppError::new(
                ErrorClass::InvalidState,
                "document.invalid_state",
                format!("`{}` rows live in their parent's tables.", def.name),
            ));
        }
        if !doc.is_new() {
            return Err(state_error("Creating a document that was already saved"));
        }
        let principal = self
            .authorize(inv, Action::Op(Operation::Create), &doc)
            .await?;
        let changes = create_changes(&doc, def.as_ref());
        self.authorize_write(&principal, Action::Op(Operation::Create), &doc, &changes)
            .await?;
        self.run_op(snap, work, inv, Operation::Create, doc).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn do_update(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> AppResult<Document> {
        let def = snap.doctype(doctype)?;
        let mut doc = self
            .load_for_update(snap, work, doctype, id, StorageOp::Write)
            .await?;
        if doc.version() != Some(expected) {
            return Err(stale(doc.version()));
        }
        let op = match doc.state() {
            Some(DocState::Posted) => {
                refuse_fixed_fields(&def, &patch)?;
                Operation::UpdatePosted
            }
            Some(DocState::Cancelled) => {
                return Err(state_error("Changing a cancelled document"));
            }
            _ => Operation::Update,
        };
        let ids = &self.ids;
        // The operation gate runs on the loaded document (owner, scope and state are real)
        // before any step; the field and link checks run on the patched document, so moving a
        // link outside the user's scope is caught by `check_links`.
        let action = match op {
            Operation::UpdatePosted => Action::Op(Operation::UpdatePosted),
            _ => Action::Op(Operation::Update),
        };
        let principal = self.authorize(inv, action, &doc).await?;
        // The document as stored, for the participants (WP-DOC-04.T3): the patch is applied to
        // `doc` below.
        let loaded = doc.clone();
        doc.apply_patch(&patch, || ids.next_id())?;
        let changes = patch_changes(&patch, def.as_ref());
        self.authorize_write(&principal, action, &doc, &changes)
            .await?;
        self.run_op_loaded(snap, work, inv, op, doc, Some(loaded))
            .await
    }

    /// `set_secret`: the update lifecycle of a record whose only change is its sealed columns.
    ///
    /// There is no principal gate: holding the [`SecretAccess`](crate::secrets::SecretAccess)
    /// is the permission. The document must be in a state that allows an update, and the
    /// expected version must be the stored one, like every other write.
    #[allow(clippy::too_many_arguments)]
    async fn do_set_secrets(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        mut writes: SealedWrites,
        expected: Version,
    ) -> AppResult<Document> {
        let audit = std::mem::take(&mut writes.audit);
        let doc = self
            .load_for_update(snap, work, doctype, id, StorageOp::Write)
            .await?;
        if doc.version() != Some(expected) {
            return Err(stale(doc.version()));
        }
        if !matches!(doc.state(), None | Some(DocState::Draft)) {
            return Err(state_error("Setting a secret"));
        }
        work.pending_secrets = Some(PendingSecrets {
            doctype: doctype.to_owned(),
            id,
            writes,
        });
        let result = self.run_op(snap, work, inv, Operation::Update, doc).await;
        // Whatever happened, the sealed columns are not for any later write.
        work.pending_secrets = None;
        let doc = result?;
        enqueue_audit(work, &audit).await?;
        Ok(doc)
    }

    /// `create_with_secrets`: the create lifecycle with the sealed columns in the same insert.
    ///
    /// Unlike `set_secret`, the record is authorised like any create (operation gate and field
    /// write check on the document's non-secret fields): the capability authorises the secrets,
    /// not the record.
    async fn do_create_with_secrets(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doc: Document,
        mut writes: SealedWrites,
    ) -> AppResult<Document> {
        let audit = std::mem::take(&mut writes.audit);
        let def = snap.doctype(doc.doctype().as_str())?;
        if def.is_child {
            return Err(AppError::new(
                ErrorClass::InvalidState,
                "document.invalid_state",
                format!("`{}` rows live in their parent's tables.", def.name),
            ));
        }
        if !doc.is_new() {
            return Err(state_error("Creating a document that was already saved"));
        }
        // The record is an ordinary create: the operation gate and the per-field write check
        // run exactly as in `do_create`, on the document's own (non-secret) fields. Only the
        // sealed columns, which no principal can name, come from the capability.
        let principal = self
            .authorize(inv, Action::Op(Operation::Create), &doc)
            .await?;
        let changes = create_changes(&doc, def.as_ref());
        self.authorize_write(&principal, Action::Op(Operation::Create), &doc, &changes)
            .await?;
        work.pending_secrets = Some(PendingSecrets {
            doctype: doc.doctype().as_str().to_owned(),
            id: doc.id(),
            writes,
        });
        let result = self.run_op(snap, work, inv, Operation::Create, doc).await;
        work.pending_secrets = None;
        let doc = result?;
        enqueue_audit(work, &audit).await?;
        Ok(doc)
    }

    /// Merges a record's secret columns into the row about to be written.
    ///
    /// A document holds no secret, but a storage write is full: an update must carry the sealed
    /// columns the row already has (read under the lock the operation holds), or an ordinary
    /// edit would unset every credential. A pending `set_secret` / `create_with_secrets` of
    /// this same record then replaces the columns it names. Returns the names of the secret
    /// fields that were set, for the event.
    async fn merge_secret_columns(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        doc: &Document,
        stored: &mut spot_storage_contract::DocumentData,
        creating: bool,
    ) -> AppResult<Vec<String>> {
        let doctype = doc.doctype().as_str();
        let pending = if work
            .pending_secrets
            .as_ref()
            .is_some_and(|p| p.doctype == doctype && p.id == doc.id())
        {
            work.pending_secrets.take()
        } else {
            None
        };
        let resolved = resolve(&snap.meta, doctype)?;
        let specs: Vec<_> = secret_fields(resolved.def).collect();
        if specs.is_empty() {
            return if pending.is_some() {
                Err(internal(
                    "secret columns for a DocType with no secret field",
                ))
            } else {
                Ok(Vec::new())
            };
        }
        if !creating {
            let existing = work
                .tx
                .load(doc.doctype(), doc.id(), RowLock::None)
                .await
                .map_err(|e| work.fail(e, StorageOp::Write))?
                .ok_or_else(not_found)?;
            for spec in &specs {
                for column in secret_columns(spec)? {
                    if let Some(value) = existing.data.fields.get(&column) {
                        stored.fields.insert(column, value.clone());
                    }
                }
            }
        }
        let Some(pending) = pending else {
            return Ok(Vec::new());
        };
        for (column, value) in pending.writes.columns {
            if !stored.fields.contains_key(&column) {
                return Err(internal("a sealed column the DocType does not declare"));
            }
            stored.fields.insert(column, value);
        }
        Ok(pending.writes.fields)
    }

    async fn do_delete(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        expected: Version,
    ) -> AppResult<Document> {
        let doc = self
            .load_for_update(snap, work, doctype, id, StorageOp::Delete)
            .await?;
        if doc.version() != Some(expected) {
            return Err(stale(doc.version()));
        }
        if !matches!(doc.state(), None | Some(DocState::Draft)) {
            return Err(state_error("delete"));
        }
        self.authorize(inv, Action::Op(Operation::Delete), &doc)
            .await?;
        self.run_op(snap, work, inv, Operation::Delete, doc).await
    }

    /// Post or cancel a document: checks the state gate, then runs the operation's lifecycle.
    ///
    /// `post` runs only on a posted DocType's draft and `cancel` only on its posted documents;
    /// every other state is `document.invalid_state`.
    #[allow(clippy::too_many_arguments)]
    async fn do_transition(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        op: Operation,
        doctype: &str,
        id: RecordId,
        expected: Version,
    ) -> AppResult<Document> {
        let def = snap.doctype(doctype)?;
        let doc = self
            .load_for_update(snap, work, doctype, id, StorageOp::Write)
            .await?;
        if doc.version() != Some(expected) {
            return Err(stale(doc.version()));
        }
        let allowed = match op {
            Operation::Post => def.is_posted && doc.state() == Some(DocState::Draft),
            Operation::Cancel => doc.state() == Some(DocState::Posted),
            _ => true,
        };
        if !allowed {
            return Err(state_error(op_name(op)));
        }
        self.authorize(inv, Action::Op(op), &doc).await?;
        self.run_op(snap, work, inv, op, doc).await
    }

    /// Correct a posted document (ADR-42): cancel it and create its amended draft in **one**
    /// transaction, in [`correct_sequence`](spot_app_api::lifecycle::correct_sequence) order.
    /// A stale version or a document that is not posted changes nothing.
    async fn do_correct(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        expected: Version,
    ) -> AppResult<Document> {
        let def = snap.doctype(doctype)?;
        let original = self
            .load_for_update(snap, work, doctype, id, StorageOp::Write)
            .await?;
        if original.version() != Some(expected) {
            return Err(stale(original.version()));
        }
        if original.state() != Some(DocState::Posted) {
            return Err(state_error(op_name(Operation::Cancel)));
        }
        // Correct is cancel and amend: it needs both the `cancel` and the `create` right
        // (ADR-42). The amendment's own create gate runs when it is created below.
        let outer = std::mem::replace(&mut work.cause, Cause::CorrectCancel);
        self.authorize(inv, Action::Correct, &original).await?;
        // Cancel the original: its phases 1–3 run here and its committed step is queued.
        let cancelled = self
            .run_op(snap, work, inv, Operation::Cancel, original)
            .await?;
        // The amended draft: a new record with new row ids, the same values and `amended_from`.
        let mut draft = Document::new(cancelled.doctype().clone(), self.ids.next_id());
        for (field, value) in cancelled.fields() {
            draft.set(field.clone(), value.clone());
        }
        for (table, rows) in cancelled.tables() {
            let fresh: Vec<ChildRow> = rows
                .iter()
                .map(|row| ChildRow {
                    id: self.ids.next_id(),
                    fields: row.fields.clone(),
                })
                .collect();
            *draft.rows_mut(table.clone()) = fresh;
        }
        // A generated name (series, hash) is never copied: the amendment is named like any
        // new document.
        if let Some(title) = snap.naming.generated_title(doctype) {
            draft.set(title.clone(), Value::Null);
        }
        draft.set(name(AMENDED_FROM)?, Value::Id(cancelled.id()));
        // A name the document itself carries (`field:` or `prompt`) survives as an amendment:
        // the cancelled original still holds it, so the copy takes `<base>-<n>`, chosen here
        // under the name lock and consumed by the copy's own naming below.
        if let Some(title) = snap.naming.amended_title(doctype) {
            let original_name = title_text(&def, &cancelled);
            work.amended_name = Some(
                self.amendment_name(work, &def, title, &original_name)
                    .await?,
            );
        }
        // Create the draft: its phases 1–3 run here, in the same transaction. A failure keeps
        // this cause for the denial the ledger reports (it is the amend half that was refused).
        work.cause = Cause::CorrectAmend;
        let amended = self.do_create(snap, work, inv, draft).await?;
        work.cause = outer;
        Ok(amended)
    }

    /// Rename a document: set its title field and put the old and new names in the event.
    #[allow(clippy::too_many_arguments)]
    async fn do_rename(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        new_name: &str,
        expected: Version,
    ) -> AppResult<Document> {
        let def = snap.doctype(doctype)?;
        let title_field = def
            .title_field
            .clone()
            .ok_or_else(|| state_error("Renaming a DocType without a title field"))?;
        // A generated name (series or hash) is not a title the caller owns: there is nothing to
        // rewrite, and the number or hash would no longer match the document. Refused before
        // anything is read or written.
        if snap.naming.generates_name(doctype) {
            return Err(naming::name_not_renameable(&def, &title_field));
        }
        let mut doc = self
            .load_for_update(snap, work, doctype, id, StorageOp::Write)
            .await?;
        if doc.version() != Some(expected) {
            return Err(stale(doc.version()));
        }
        self.check_rename_free(snap, work, inv, &def, id, new_name)
            .await?;
        work.rename_from = Some(title_text(&def, &doc));
        let title_path = title_field.as_str().to_owned();
        let action = Action::Op(Operation::Rename);
        let principal = self.authorize(inv, action, &doc).await?;
        let loaded = doc.clone();
        doc.set(title_field, Value::Text(new_name.to_owned()));
        self.authorize_write(&principal, action, &doc, &[WriteChange::set(title_path)])
            .await?;
        self.run_op_loaded(snap, work, inv, Operation::Rename, doc, Some(loaded))
            .await
    }

    /// Call a whitelisted method on a document (ADR-16): the method runs in its own transaction
    /// with a [`MethodContext`](spot_app_api::MethodContext), may read and write through the
    /// repository (including its own document, through the update lifecycle), and its JSON
    /// result is returned with the document as it is afterwards.
    ///
    /// When `expected` is `Some`, the document must be at that version
    /// ([`document.stale_version`](ErrorClass::StaleVersion) otherwise); when it is `None` only
    /// existence is checked.
    #[allow(clippy::too_many_arguments)]
    async fn do_method(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        method: &str,
        args: Json,
        expected: Option<Version>,
    ) -> AppResult<(Document, Json)> {
        // One answer for a method that does not exist, a document that does not exist and a
        // document the principal may not read: `method.not_found`, so a caller without `read`
        // learns neither that the method nor that the document exists.
        let unknown = || {
            AppError::new(
                ErrorClass::MethodNotFound,
                "method.not_found",
                format!("The DocType `{doctype}` has no whitelisted method `{method}`."),
            )
        };
        let controller = self.find_method(doctype, method).ok_or_else(unknown)?;
        // The declaration the right comes from. None cannot happen (`find_method` just matched
        // it); if it does the call is refused, never run with a default right.
        let decl = controller
            .methods()
            .into_iter()
            .find(|m| m.name.as_str() == method)
            .ok_or_else(unknown)?;
        let doc = self
            .load_for_update(snap, work, doctype, id, StorageOp::Other)
            .await
            .map_err(|e| {
                if e.code == "document.not_found" {
                    unknown()
                } else {
                    e
                }
            })?;
        // `read` first: without it nothing about the document or the method is revealed.
        let principal = self.acting_principal(inv).await;
        if let Err(error) = self
            .authorizer
            .authorize(&principal, Action::Read, &doc)
            .await
        {
            if error.class != ErrorClass::PermissionDenied {
                return Err(error);
            }
            self.record_masked_denial(&error);
            work.trail.masked = Some(error);
            return Err(unknown());
        }
        if let Some(expected) = expected
            && doc.version() != Some(expected)
        {
            return Err(stale(doc.version()));
        }
        // The right the method declares (ADR-16, app API 0.6.0), checked before the body runs
        // and before any write.
        self.authorize_method(inv, &decl, &doc).await?;
        let point = HookPoint::of(Phase::After, Target::Op(Operation::Update));
        work.method = Some(method.to_owned());
        let mut ctx = Ctx::method(self, snap, work, inv, point, doc);
        let result = controller.call_method(method, args, &mut ctx).await;
        let messages = std::mem::take(&mut ctx.messages);
        drop(ctx);
        work.method = None;
        work.messages.extend(messages);
        let result = result?;
        let after = self
            .load_stored(snap, work, doctype, id, RowLock::None, StorageOp::Other)
            .await?
            .ok_or_else(not_found)?;
        Ok((after, result))
    }

    /// Loads the operation's document with the row locked until the transaction ends.
    async fn load_for_update(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        doctype: &str,
        id: RecordId,
        during: StorageOp,
    ) -> AppResult<Document> {
        self.load_stored(
            snap,
            work,
            doctype,
            id,
            RowLock::Update(Wait::Block),
            during,
        )
        .await?
        .ok_or_else(not_found)
    }

    /// Loads a document through the open transaction.
    pub(crate) async fn load_stored(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        doctype: &str,
        id: RecordId,
        lock: RowLock,
        during: StorageOp,
    ) -> AppResult<Option<Document>> {
        snap.doctype(doctype)?;
        let entity = name(doctype)?;
        let stored = work
            .tx
            .load(&entity, id, lock)
            .await
            .map_err(|e| work.fail(e, during))?;
        stored.map(|s| from_storage(&snap.meta, s)).transpose()
    }

    /// Runs one operation's stages inside the open transaction and returns the document as
    /// written (for a delete, as it was before). The effects its steps report are collected
    /// into the operation's receipt, which is closed once the `after_*` steps are done.
    async fn run_op(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        op: Operation,
        doc: Document,
    ) -> AppResult<Document> {
        self.run_op_loaded(snap, work, inv, op, doc, None).await
    }

    /// [`run_op`](Self::run_op) with the document as storage held it, when the caller changed
    /// the document before this point (update and rename). With `None` the document passed in
    /// is the loaded one (post, cancel, delete) or there is none (create).
    async fn run_op_loaded(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        op: Operation,
        doc: Document,
        loaded: Option<Document>,
    ) -> AppResult<Document> {
        let loaded = match op {
            Operation::Create => None,
            _ => loaded.or_else(|| Some(doc.clone())),
        };
        work.collectors.push(ReceiptCollector::default());
        let result = self.run_stages(snap, work, inv, op, doc, loaded).await;
        let collected = work.collectors.pop().unwrap_or_default();
        let doc = result?;
        self.finish_receipt(snap, work, inv, op, &doc, collected)
            .await?;
        Ok(doc)
    }

    /// The stages of [`run_op`](Self::run_op).
    async fn run_stages(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        op: Operation,
        doc: Document,
        loaded: Option<Document>,
    ) -> AppResult<Document> {
        let def = snap.doctype(doc.doctype().as_str())?;
        let participants = self.participants();
        let mut doc = doc;
        let before_delete = (op == Operation::Delete).then(|| doc.clone());
        if op != Operation::Delete {
            if op == Operation::Create {
                defaults::apply(&snap.meta, &mut doc, inv.now)?;
            }
            coerce::normalise(&snap.meta, &mut doc)?;
        }
        let mut derived = false;
        // One `doc.phase` span per phase run, shared by the hook points of the same phase
        // (`before_create` and `before_save` are both phase `before`).
        let mut phase_span: Option<(&'static str, Span)> = None;
        for stage in sequence(op) {
            match stage {
                Stage::Hook(point) if point.phase() == Phase::Committed => {
                    let seen = before_delete.clone().unwrap_or_else(|| doc.clone());
                    work.committed.push(CommittedItem {
                        doc: seen,
                        point,
                        operation: op,
                    });
                }
                Stage::Hook(point) => {
                    if point.phase() == Phase::Validate && !derived && op != Operation::Delete {
                        // Phase 1 may have derived values of the wrong numeric type.
                        if op == Operation::Create {
                            // Named inside this transaction, after the steps that derive
                            // values and before the checks: a refusal below rolls the
                            // number back with everything else.
                            doc = self.assign_name(snap, work, inv, &def, doc).await?;
                        }
                        coerce::normalise(&snap.meta, &mut doc)?;
                        derived = true;
                    }
                    let span = phase_in(&mut phase_span, point.phase().as_str());
                    doc = self
                        .run_hook(snap, work, inv, point, doc)
                        .instrument(span)
                        .await?;
                }
                Stage::Write => {
                    if op == Operation::Post {
                        // The posting engine's seam: ledger entries are written after the
                        // `validate_post` steps and before the state change, in this same
                        // transaction (ERP ledger WPs).
                        self.write_ledger_entries(snap, work, inv, &doc).await?;
                    }
                    let span = phase_in(&mut phase_span, "write");
                    doc = self
                        .write(
                            snap,
                            work,
                            inv,
                            op,
                            &def,
                            doc,
                            (&participants, loaded.as_ref()),
                        )
                        .instrument(span)
                        .await?;
                }
                Stage::Commit => {}
            }
        }
        Ok(before_delete.unwrap_or(doc))
    }

    /// Runs the steps at one hook point, in the transaction, and returns the document as the
    /// steps left it.
    async fn run_hook(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        point: HookPoint,
        doc: Document,
    ) -> AppResult<Document> {
        let mut ctx = Ctx::in_tx(self, snap, work, inv, point, doc);
        let result = ctx.run_and_flush().await;
        let reports = ctx.take_reports();
        let Ctx { doc, messages, .. } = ctx;
        result?;
        work.messages.extend(messages);
        work.collect(reports);
        Ok(doc)
    }

    /// The posting engine's seam (ERP ledger WPs): a document's ledger entries.
    ///
    /// [`write`](Self::write) calls it for `post`, **between** the `validate_post` steps and the
    /// state change, in the same transaction, so entries commit with the post or not at all. It
    /// is empty until the ledger work packages implement it, and it never changes the document's
    /// state.
    async fn write_ledger_entries(
        &self,
        _snap: &Snapshot,
        _work: &mut Work<B::Tx>,
        _inv: &Invocation,
        _doc: &Document,
    ) -> AppResult<()> {
        Ok(())
    }

    /// The write stage: the checks, the storage write, the version, and the lifecycle event.
    ///
    /// Every document write of every operation passes here (create, update, post, cancel,
    /// delete, rename, update-posted, both halves of a correction, and the writes a method or a
    /// step makes), so this is the one place the transaction participants run (WP-DOC-04.T3):
    /// `before_write` ahead of the storage write, `after_write` right after it and its event,
    /// both before any `after_*` step, so a participant failure fails the whole operation.
    #[allow(clippy::too_many_arguments)]
    async fn write(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        op: Operation,
        def: &DocTypeDef,
        mut doc: Document,
        (participants, loaded): (&Participants<B::Tx>, Option<&Document>),
    ) -> AppResult<Document> {
        let during = if op == Operation::Delete {
            StorageOp::Delete
        } else {
            StorageOp::Write
        };
        let cause = work.cause.clone();
        let node = self.node.as_ref().map(NodeId::as_str);
        if op == Operation::Delete {
            let version = doc
                .version()
                .ok_or_else(|| internal("deleting a document that was never saved"))?;
            // Documents that still refer to this one: a refusal that lists them.
            self.check_unreferenced(snap, work, inv, def, op, &doc)
                .await?;
            let intent = WriteIntent {
                doctype: doc.doctype().as_str(),
                operation: op,
                cause: cause.view(),
                node,
                old: loaded,
                new: None,
                invocation: inv,
            };
            let refused = run_before(participants, &mut work.tx, &intent).await;
            work.note_participant(refused)?;
            if let Err(e) = work.tx.delete(doc.doctype(), doc.id(), version).await {
                // A reference created after the look-up above: still `links.referenced`.
                let violation = e.kind == ErrorKind::ReferenceViolation;
                let mapped = work.fail(e, during);
                return Err(if violation {
                    links::delete_refusal(links::referenced_refusal(def, doc.id()))
                } else {
                    mapped
                });
            }
            self.capture_deleted(work, &doc);
            work.deleted = true;
            work.deleted_target
                .get_or_insert_with(|| (doc.doctype().as_str().to_owned(), doc.id()));
            let data = json!({"name": title(def, &doc)});
            let event = outbox::lifecycle(&*self.ids, inv, op, &doc, Some(version), data)?;
            work.tx
                .enqueue(&event)
                .await
                .map_err(|e| work.fail(e, during))?;
            let record = WriteRecord {
                doctype: doc.doctype().as_str(),
                operation: op,
                cause: cause.view(),
                node,
                old: loaded,
                new: None,
                version: None,
                invocation: inv,
            };
            let refused = run_after(participants, &mut work.tx, &record).await;
            work.note_participant(refused)?;
            return Ok(doc);
        }

        validate::check(&snap.meta, &doc)?;
        // Link integrity, in this transaction and before the write: no dangling link is stored.
        self.check_write_links(snap, work, inv, def, op, &doc)
            .await?;
        if op == Operation::Cancel {
            self.check_unreferenced(snap, work, inv, def, op, &doc)
                .await?;
        }
        let intent = WriteIntent {
            doctype: doc.doctype().as_str(),
            operation: op,
            cause: cause.view(),
            node,
            old: loaded,
            new: Some(&doc),
            invocation: inv,
        };
        let refused = run_before(participants, &mut work.tx, &intent).await;
        work.note_participant(refused)?;
        let state = match op {
            Operation::Create => initial_state(def),
            Operation::Post => Some(DocState::Posted),
            Operation::Cancel => Some(DocState::Cancelled),
            // Update, update_posted and rename keep the state the document already has.
            _ => doc.state(),
        };
        let mut changed: Vec<String> = doc
            .changed_fields()
            .iter()
            .map(|n| n.as_str().to_owned())
            .collect();
        let version = if op == Operation::Create {
            let mut stored = to_storage(&snap.meta, &doc)?;
            self.merge_secret_columns(snap, work, &doc, &mut stored, true)
                .await?;
            work.tx
                .insert(&stored)
                .await
                .map_err(|e| work.fail(e, during))?
        } else {
            let expected = doc
                .version()
                .ok_or_else(|| internal("writing a document that was never saved"))?;
            // The storage mapping writes `doc_state` from the document, so the new state must be
            // on the document before it; `expected` is still the loaded version.
            if state != doc.state() {
                doc.mark_saved(expected, state);
            }
            let mut stored = to_storage(&snap.meta, &doc)?;
            // Names only: a secret's name is in the event, never its value.
            changed.extend(
                self.merge_secret_columns(snap, work, &doc, &mut stored, false)
                    .await?,
            );
            work.tx
                .update(&stored, expected)
                .await
                .map_err(|e| work.fail(e, during))?
        };
        doc.mark_saved(version, state);
        self.capture_wrote(work, op == Operation::Create, &doc);
        let data = match op {
            Operation::Update | Operation::UpdatePosted => {
                json!({"name": title(def, &doc), "changed_fields": changed})
            }
            Operation::Rename => json!({
                "old_name": work.rename_from.take().unwrap_or_default(),
                "new_name": title(def, &doc),
            }),
            _ => json!({"name": title(def, &doc)}),
        };
        let event = outbox::lifecycle(&*self.ids, inv, op, &doc, Some(version), data)?;
        work.tx
            .enqueue(&event)
            .await
            .map_err(|e| work.fail(e, during))?;
        let record = WriteRecord {
            doctype: doc.doctype().as_str(),
            operation: op,
            cause: cause.view(),
            node,
            old: loaded,
            new: Some(&doc),
            version: Some(version),
            invocation: inv,
        };
        let refused = run_after(participants, &mut work.tx, &record).await;
        work.note_participant(refused)?;
        Ok(doc)
    }

    // -------------------------------------------------------------------------------------
    // Reads and enqueues outside a business transaction
    // -------------------------------------------------------------------------------------

    /// A document through a short read-only transaction (reads from `load` and from steps that
    /// run outside a transaction).
    pub(crate) async fn read_document(
        &self,
        snap: &Snapshot,
        doctype: &str,
        id: RecordId,
    ) -> AppResult<Option<Document>> {
        snap.doctype(doctype)?;
        let entity = name(doctype)?;
        let label = label(doctype, "load");
        self.retrying(&label, || async {
            let mut tx = self
                .backend
                .begin(
                    snap.catalog.clone(),
                    TxOptions::new(label.clone(), Isolation::ReadCommitted).read_only(),
                )
                .await
                .map_err(|e| Failed::Storage(e, StorageOp::Other))?;
            let stored = tx.load(&entity, id, RowLock::None).await;
            let _ = tx.rollback().await;
            let stored = stored.map_err(|e| Failed::Storage(e, StorageOp::Other))?;
            stored
                .map(|s| from_storage(&snap.meta, s))
                .transpose()
                .map_err(Failed::App)
        })
        .await
    }

    /// A query through a short read-only transaction.
    pub(crate) async fn query_detached(&self, snap: &Snapshot, query: &Query) -> AppResult<Rows> {
        let label = label(query.from.entity.as_str(), "query");
        self.retrying(&label, || async {
            let mut tx = self
                .backend
                .begin(
                    snap.catalog.clone(),
                    TxOptions::new(label.clone(), Isolation::ReadCommitted).read_only(),
                )
                .await
                .map_err(|e| Failed::Storage(e, StorageOp::Other))?;
            let rows = tx.query(query).await;
            let _ = tx.rollback().await;
            rows.map_err(|e| Failed::Storage(e, StorageOp::Other))
        })
        .await
    }

    /// Writes events to the outbox in a transaction of their own: what a step enqueues after
    /// the business transaction committed (phase 4).
    pub(crate) async fn enqueue_detached(
        &self,
        snap: &Snapshot,
        events: Vec<OutboxEvent>,
    ) -> AppResult<()> {
        let label = label("outbox", "enqueue");
        self.retrying(&label, || async {
            let mut tx = self
                .backend
                .begin(
                    snap.catalog.clone(),
                    TxOptions::new(label.clone(), Isolation::ReadCommitted),
                )
                .await
                .map_err(|e| Failed::Storage(e, StorageOp::Other))?;
            for event in &events {
                if let Err(e) = tx.enqueue(event).await {
                    let _ = tx.rollback().await;
                    return Err(Failed::Storage(e, StorageOp::Other));
                }
            }
            tx.commit()
                .await
                .map_err(|e| Failed::Storage(e, StorageOp::Other))
        })
        .await
    }

    // -------------------------------------------------------------------------------------
    // Load and form events
    // -------------------------------------------------------------------------------------

    /// Loads a document and runs its `load` steps, read-only, as the invocation's principal:
    /// the read is refused unless allowed, and the result is filtered like every other read.
    pub(crate) async fn load_document(
        &self,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
    ) -> AppResult<Outcome<Document>> {
        let snap = self.snapshot();
        let doc = self
            .read_document(&snap, doctype, id)
            .await?
            .ok_or_else(not_found)?;
        let principal = self.authorize(inv, Action::Read, &doc).await?;
        let mut ctx = Ctx::detached(self, &snap, inv, HookPoint::Load, doc);
        ctx.run_point().await?;
        let doc = self.readable(&principal, ctx.doc).await?;
        Ok(Outcome {
            value: doc,
            messages: ctx.messages,
        })
    }

    /// Runs `field_changed:<field>` steps on an unsaved draft; storage is not touched.
    pub(crate) async fn field_changed_draft(
        &self,
        inv: &Invocation,
        draft: Document,
        field: &str,
    ) -> AppResult<Outcome<Document>> {
        let snap = self.snapshot();
        snap.doctype(draft.doctype().as_str())?;
        let point = HookPoint::FieldChanged(field.to_owned());
        let mut ctx = Ctx::detached(self, &snap, inv, point, draft);
        ctx.run_point().await?;
        Ok(Outcome {
            value: ctx.doc,
            messages: ctx.messages,
        })
    }
}

/// The fields of a patch that may not change on a posted document, as one validation error.
fn refuse_fixed_fields(def: &DocTypeDef, patch: &DocumentPatch) -> AppResult<()> {
    let fixed: Vec<&Name> = patch
        .fields
        .keys()
        .chain(patch.tables.keys())
        .filter(|k| !def.field(k.as_str()).is_some_and(|f| f.allow_on_posted))
        .collect();
    let Some((first, rest)) = fixed.split_first() else {
        return Ok(());
    };
    let msg = "The field cannot be changed on a posted document.";
    let code = "document.read_only_field";
    let error = rest.iter().fold(
        AppError::field(FieldPath::field(first.as_str()), code, msg),
        |e, k| e.with_field(FieldPath::field(k.as_str()), code, msg),
    );
    Err(error)
}

/// The batch of committed steps an operation collected.
fn batch_of(inv: &Invocation, committed: &[CommittedItem]) -> CommittedBatch {
    CommittedBatch {
        invocation: inv.clone(),
        steps: committed
            .iter()
            .map(|c| CommittedStep {
                point: c.point.clone(),
                doc: c.doc.clone(),
                operation: c.operation,
            })
            .collect(),
    }
}

/// The transaction's outbox, lent to a delivery path as an object-safe sink. A storage error is
/// kept, so the engine can retry the transaction when the error allows it.
struct TxOutbox<'t, T> {
    tx: &'t mut T,
    failed: Option<spot_storage_contract::StorageError>,
}

impl<T: spot_storage_contract::Outbox> OutboxSink for TxOutbox<'_, T> {
    fn enqueue<'a>(&'a mut self, event: &'a OutboxEvent) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            match self.tx.enqueue(event).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let error = map_storage_error(&e, StorageOp::Write);
                    self.failed.get_or_insert(e);
                    Err(error)
                }
            }
        })
    }
}

/// The committed steps of an engine, as a delivery path sees them.
pub(crate) struct EngineSteps<'a, B: StorageBackend> {
    pub(crate) engine: &'a Engine<B>,
    pub(crate) snap: &'a Snapshot,
    /// The external effects the steps announced, for the receipt returned to the caller.
    pub(crate) announced: Mutex<Vec<Announced>>,
}

impl<B: StorageBackend> CommittedSteps for EngineSteps<'_, B> {
    fn run<'a>(
        &'a self,
        invocation: &'a Invocation,
        step: CommittedStep,
    ) -> BoxFuture<'a, Vec<Message>> {
        Box::pin(async move { self.run_checked(invocation, step).await.messages })
    }

    fn run_checked<'a>(
        &'a self,
        invocation: &'a Invocation,
        step: CommittedStep,
    ) -> BoxFuture<'a, StepOutcome> {
        Box::pin(async move {
            let (point, doctype, id) = (
                step.point.name(),
                step.doc.doctype().as_str().to_owned(),
                step.doc.id(),
            );
            let mut ctx = Ctx::detached(self.engine, self.snap, invocation, step.point, step.doc)
                .for_operation(step.operation);
            let outcome = ctx.run_and_flush().await;
            self.announced
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .append(&mut ctx.announced);
            let failure = match outcome {
                Ok(()) => None,
                Err(error) => {
                    self.engine.record_failure(CommittedFailure {
                        point,
                        doctype,
                        id,
                        error: error.clone(),
                        at: self.engine.clock.now_utc(),
                    });
                    Some(error)
                }
            };
            StepOutcome {
                messages: ctx.messages,
                failure,
            }
        })
    }
}
