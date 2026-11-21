//! Operation receipts: what a lifecycle operation did, collected from the effects its steps
//! report (WP-DOC-12.T1; ADR-080 section 6.4).
//!
//! # Where a receipt lives
//!
//! The smallest option that fits ADR-056 is the outbox, which already carries the lifecycle event
//! in the business transaction. A receipt with at least one effect is written as one more outbox
//! event of the platform topic [`TOPIC_RECEIPT`], **in the same transaction**, after the `after_*`
//! steps ran, so it commits with the operation or not at all (a failure in phase 3 leaves no
//! receipt and no persisted effect). An operation whose steps report nothing writes no extra
//! event: its receipt is empty, which is valid, and it is rebuilt from the lifecycle event when
//! asked for.
//!
//! An external effect (phase 4) is announced after the commit. Each announcement is an event of
//! the topic [`TOPIC_RECEIPT_EFFECT`], about the same document and version; reading a receipt
//! folds the announcements into it. An effect is identified by its step, class, kind and digest,
//! so a second announcement of the same effect (an idempotent re-delivery) never adds an entry,
//! and moves its `state` only forward (see [`ReceiptState`]'s order below).
//!
//! # What a receipt never holds
//!
//! The engine stores what the steps report and nothing else: the step, class, kind, opaque
//! references, a count, a digest and a state. It reads no store the effects write, and it never
//! copies a field value into a receipt. A failure code a step reports is reduced to a stable
//! code shape, so a peer's text cannot ride along. The number of entries and references of one
//! operation is bounded.
//!
//! # State order
//!
//! `Applied` < `Held` < `Released` < `Delivered` and `Failed` (the last two are at the same
//! rank, so a retry may turn a failure into a delivery). A report never moves a state to a lower
//! rank.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use spot_app_api::context::{Invocation, Outcome};
use spot_app_api::error::{StorageOp, map_storage_error};
use spot_app_api::prelude::{OffsetDateTime, RecordId, Version};
use spot_app_api::{
    AppError, AppResult, Document, DocumentPatch, EffectClassName, EffectKind, EffectReceipt,
    ErrorClass, Hex32, Operation, OperationReceipt, ReceiptState, StepName,
};
use spot_events_contract::PayloadDocument;
use spot_events_contract::lifecycle::Operation as EventOperation;
use spot_permissions::Action;
use spot_seams::{Undeclared, UndeclaredReason, check_receipt};
use spot_storage_contract::{FeedPosition, Outbox, OutboxEvent, OutboxRecord, StorageBackend};

use super::ops::{Done, not_found};
use super::work::Work;
use super::{CommittedFailure, Engine, Snapshot, lock, outbox};

/// The platform topic of an operation's receipt, written in the business transaction when the
/// receipt has at least one effect. `data` is the receipt in its contract wire form, plus
/// `undeclared`, the indexes of the entries no declaration covers.
pub const TOPIC_RECEIPT: &str = "sys_operation_receipt";

/// The platform topic of one external effect announced after the commit (phase 4). `data` holds
/// `operation`, `effect` (an effect entry) and `undeclared`.
pub const TOPIC_RECEIPT_EFFECT: &str = "sys_operation_receipt_effect";

/// Most distinct effect entries one operation's receipt may hold.
pub const MAX_EFFECTS_PER_RECEIPT: usize = 256;

/// Most opaque references all the entries of one operation's receipt may hold together.
pub const MAX_REFS_PER_RECEIPT: usize = 4096;

/// Undeclared-effect records kept for inspection; older ones are dropped.
const UNDECLARED_LOG: usize = 256;

/// The longest failure code a receipt keeps.
const MAX_CODE_LEN: usize = 64;

/// The failure code kept when the one a step reported is not a stable code.
const REPORTED_FAILURE: &str = "reported_failure";

/// Feed page size when a receipt is read back.
const PAGE: u32 = 500;

/// A receipt as the engine holds it: the contract's [`OperationReceipt`] and which of its entries
/// no declaration covers.
///
/// The contract's entry has no room for a flag, so the engine keeps it beside the receipt: the
/// indexes in [`undeclared`](Self::undeclared) refer to [`OperationReceipt::effects`]. With
/// `strict_effects` off, an undeclared effect is kept and flagged; with it on, the operation
/// fails instead and no record exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptRecord {
    /// The receipt.
    pub receipt: OperationReceipt,
    /// Indexes into `receipt.effects` of the entries that were reported without a declaration.
    pub undeclared: Vec<usize>,
}

impl ReceiptRecord {
    /// A record with no effects.
    pub(crate) fn empty(
        operation: Operation,
        doctype: &str,
        name: String,
        version: Version,
        committed_at: OffsetDateTime,
    ) -> Self {
        Self {
            receipt: OperationReceipt {
                operation,
                doctype: doctype.to_owned(),
                name,
                version,
                effects: Vec::new(),
                committed_at,
            },
            undeclared: Vec::new(),
        }
    }

    /// Whether entry `index` was reported without a declaration.
    #[must_use]
    pub fn is_undeclared(&self, index: usize) -> bool {
        self.undeclared.contains(&index)
    }

    /// Adds an effect announced after the commit, or moves the state of the entry it repeats.
    pub(crate) fn apply(&mut self, effect: EffectReceipt, undeclared: bool) {
        let at = upsert(&mut self.receipt.effects, effect);
        if undeclared && !self.undeclared.contains(&at) {
            self.undeclared.push(at);
        }
    }

    fn matches(&self, doctype: &str, name: &str, version: Version, op: Operation) -> bool {
        self.receipt.doctype == doctype
            && self.receipt.name == name
            && self.receipt.version == version
            && self.receipt.operation == op
    }
}

/// The result of a lifecycle operation together with its receipt.
///
/// `receipt` is the operation's own: for `correct` it is the cancel of the original (a correction
/// is cancel plus a new draft, ADR-080 6.3). `nested` holds the receipts of every other
/// operation the same call ran, in the order they finished: the writes its steps made in phase 3
/// and, for `correct`, the creation of the amended draft. Each has its own receipt, because each
/// is its own lifecycle operation with its own pipeline.
#[derive(Clone, Debug, PartialEq)]
pub struct Receipted<T> {
    /// What the operation returned, as the `DocumentService` call returns it.
    pub outcome: Outcome<T>,
    /// The operation's receipt. Phase-4 effects announced during the call are included.
    pub receipt: ReceiptRecord,
    /// The receipts of the other operations of the call.
    pub nested: Vec<ReceiptRecord>,
}

/// One effect reported by a step that is not covered by a declaration (the `effects.undeclared`
/// event of ADR-080 6.4). It carries names only, so it is safe to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndeclaredEffect {
    /// The record's DocType.
    pub doctype: String,
    /// The operation.
    pub operation: Operation,
    /// The step that reported it.
    pub step: StepName,
    /// The class it reported.
    pub class: EffectClassName,
    /// The kind it reported.
    pub kind: EffectKind,
    /// Why no declaration covers it.
    pub reason: UndeclaredReason,
    /// When it was seen.
    pub at: OffsetDateTime,
}

/// The digest of the rows an effect touched: SHA-256 over a canonical encoding of their ids and
/// versions (ADR-080 6.4). Steps put it in [`EffectReceipt::digest`]; it covers no field value.
///
/// The encoding is a fixed tag, the number of rows, then every row sorted by id and version as
/// the length of its id, the id and the version, all numbers big-endian. The order the rows are
/// given in does not change the digest.
///
/// # Errors
///
/// `document.internal_error` if the digest cannot be written as a [`Hex32`], which cannot
/// happen for a SHA-256 output.
pub fn rows_digest(rows: &[(&str, Version)]) -> AppResult<Hex32> {
    let mut sorted: Vec<&(&str, Version)> = rows.iter().collect();
    sorted.sort_by(|a, b| (a.0.as_bytes(), a.1.0).cmp(&(b.0.as_bytes(), b.1.0)));
    let mut hasher = Sha256::new();
    hasher.update(b"spot.effect-rows.v1");
    hasher.update((sorted.len() as u64).to_be_bytes());
    for (id, version) in sorted {
        hasher.update((id.len() as u64).to_be_bytes());
        hasher.update(id.as_bytes());
        hasher.update(version.0.to_be_bytes());
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        hex.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        hex.push(char::from(b"0123456789abcdef"[usize::from(byte & 0x0f)]));
    }
    Hex32::new(&hex).map_err(|_| internal("a digest could not be encoded"))
}

fn internal(msg: &str) -> AppError {
    AppError::new(ErrorClass::Internal, "document.internal_error", msg)
}

fn receipt_error(what: impl std::fmt::Display) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "platform.event",
        format!("The receipt could not be handled: {what}"),
    )
}

/// The order of states: a report never lowers it.
fn rank(state: &ReceiptState) -> u8 {
    match state {
        ReceiptState::Held => 1,
        ReceiptState::Released => 2,
        ReceiptState::Delivered | ReceiptState::Failed { .. } => 3,
        _ => 0,
    }
}

/// Keeps a failure code only when it has the shape of a stable code (lowercase letters, digits,
/// dots and underscores, starting with a letter), so free text from a peer cannot be carried.
fn safe_code(code: &str) -> String {
    let stable = !code.is_empty()
        && code.len() <= MAX_CODE_LEN
        && code.starts_with(|c: char| c.is_ascii_lowercase())
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_');
    if stable {
        code.to_owned()
    } else {
        REPORTED_FAILURE.to_owned()
    }
}

fn sanitised(mut effect: EffectReceipt) -> EffectReceipt {
    if let ReceiptState::Failed { code } = &effect.state {
        effect.state = ReceiptState::Failed {
            code: safe_code(code),
        };
    }
    effect
}

/// Adds `effect`, or, when the same effect (step, class, kind and digest) is already there,
/// moves that entry's state forward. Returns the index of the entry.
fn upsert(effects: &mut Vec<EffectReceipt>, effect: EffectReceipt) -> usize {
    let effect = sanitised(effect);
    let same = |e: &EffectReceipt| {
        e.step == effect.step
            && e.class == effect.class
            && e.kind == effect.kind
            && e.digest == effect.digest
    };
    match effects.iter().position(same) {
        Some(at) => {
            if rank(&effect.state) >= rank(&effects[at].state) {
                effects[at].state = effect.state;
            }
            at
        }
        None => {
            effects.push(effect);
            effects.len() - 1
        }
    }
}

/// Collects what the steps of one operation (or one phase-4 hook point) report through
/// [`HookContext::report_effect`](spot_app_api::HookContext::report_effect).
///
/// The report method cannot fail, so a report past the bounds sets a flag instead, and
/// [`finish`](Self::finish) turns it into the operation's failure: a step that reports without
/// limit is a defect, and the operation rolls back rather than store half a receipt.
#[derive(Debug, Default)]
pub(crate) struct ReceiptCollector {
    effects: Vec<EffectReceipt>,
    refs: usize,
    overflow: bool,
}

impl ReceiptCollector {
    /// Records one report.
    pub(crate) fn report(&mut self, effect: EffectReceipt) {
        let refs = effect.store_refs.len();
        let new = !self.effects.iter().any(|e| {
            e.step == effect.step
                && e.class == effect.class
                && e.kind == effect.kind
                && e.digest == effect.digest
        });
        if new
            && (self.effects.len() >= MAX_EFFECTS_PER_RECEIPT
                || self.refs.saturating_add(refs) > MAX_REFS_PER_RECEIPT)
        {
            self.overflow = true;
            return;
        }
        if new {
            self.refs += refs;
        }
        upsert(&mut self.effects, effect);
    }

    /// Takes over what `other` collected, in order.
    pub(crate) fn absorb(&mut self, other: ReceiptCollector) {
        self.overflow |= other.overflow;
        for effect in other.effects {
            self.report(effect);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// The effects reported, in order, or the failure for a report past the bounds.
    pub(crate) fn finish(self) -> AppResult<Vec<EffectReceipt>> {
        if self.overflow {
            return Err(receipt_error(format!(
                "a step reported more than {MAX_EFFECTS_PER_RECEIPT} effects or {MAX_REFS_PER_RECEIPT} references"
            )));
        }
        Ok(self.effects)
    }
}

/// A receipt the transaction has finished, with how deeply nested its operation ran.
pub(crate) struct Finished {
    pub(crate) record: ReceiptRecord,
    pub(crate) depth: u32,
}

/// An external effect a phase-4 step announced, waiting to be merged into the receipt returned
/// to the caller.
pub(crate) struct Announced {
    pub(crate) doctype: String,
    pub(crate) name: String,
    pub(crate) version: Version,
    pub(crate) operation: Operation,
    pub(crate) effect: EffectReceipt,
    pub(crate) undeclared: bool,
}

impl Done {
    /// Splits the receipts of the call into the operation's own and the others.
    pub(crate) fn receipts(&mut self) -> AppResult<(ReceiptRecord, Vec<ReceiptRecord>)> {
        let mut all = std::mem::take(&mut self.receipts);
        let primary = all
            .iter()
            .position(|f| f.depth == 0)
            .ok_or_else(|| internal("the operation produced no receipt"))?;
        let first = all.remove(primary);
        Ok((
            first.record,
            all.into_iter().map(|f| f.record).collect::<Vec<_>>(),
        ))
    }
}

/// Merges the effects announced during delivery into the receipts returned to the caller.
pub(crate) fn merge_announced(finished: &mut [Finished], announced: Vec<Announced>) {
    for a in announced {
        if let Some(f) = finished.iter_mut().find(|f| {
            f.record
                .matches(&a.doctype, &a.name, a.version, a.operation)
        }) {
            f.record.apply(a.effect, a.undeclared);
        }
    }
}

/// What the operation methods give besides the receipt: the same results as the
/// [`DocumentService`] methods.
fn receipted<T>(
    mut done: Done,
    into: impl FnOnce(Done) -> AppResult<Outcome<T>>,
) -> AppResult<Receipted<T>> {
    let (receipt, nested) = done.receipts()?;
    let outcome = into(done)?;
    Ok(Receipted {
        outcome,
        receipt,
        nested,
    })
}

impl<B: StorageBackend> Engine<B> {
    /// The same engine failing an operation whose steps report an effect no declaration covers
    /// (`strict_effects`), instead of keeping it flagged.
    ///
    /// The default is on for this crate's own unit tests and off otherwise, so an app's own test
    /// suite (the effect conformance kit, which deliberately reports undeclared effects) is not
    /// failed by the engine unless the host opts in. Production keeps the entry, flags it and
    /// records it in [`Engine::undeclared_effects`]. The strict failure is
    /// `document.internal_error` naming the step and class; a dedicated `effects.undeclared`
    /// code needs a catalogue entry first.
    #[must_use]
    pub fn with_strict_effects(mut self, strict: bool) -> Self {
        self.strict_effects = strict;
        self
    }

    /// The undeclared effects seen so far (the last 256), oldest first.
    pub fn undeclared_effects(&self) -> Vec<UndeclaredEffect> {
        lock(&self.undeclared).clone()
    }

    fn log_undeclared(&self, doctype: &str, operation: Operation, found: &[Undeclared]) {
        let at = self.clock.now_utc();
        let mut log = lock(&self.undeclared);
        for u in found {
            if log.len() >= UNDECLARED_LOG {
                log.remove(0);
            }
            log.push(UndeclaredEffect {
                doctype: doctype.to_owned(),
                operation,
                step: u.step.clone(),
                class: u.class.clone(),
                kind: u.kind,
                reason: u.reason,
                at,
            });
        }
    }

    /// The indexes of the entries of `receipt` that no declaration of its pipeline covers; the
    /// event is logged for each.
    fn undeclared_in(&self, snap: &Snapshot, receipt: &OperationReceipt) -> Vec<usize> {
        let found = check_receipt(&snap.seams, &receipt.doctype, receipt.operation, receipt);
        if found.is_empty() {
            return Vec::new();
        }
        self.log_undeclared(&receipt.doctype, receipt.operation, &found);
        receipt
            .effects
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                found
                    .iter()
                    .any(|u| u.step == e.step && u.class == e.class && u.kind == e.kind)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Closes the receipt of one operation inside its transaction: checks it against the
    /// declarations, writes it to the outbox when it has effects, and keeps it for the caller.
    pub(crate) async fn finish_receipt(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        op: Operation,
        doc: &Document,
        collected: ReceiptCollector,
    ) -> AppResult<()> {
        let effects = collected.finish()?;
        let version = doc
            .version()
            .ok_or_else(|| internal("an operation finished on a document without a version"))?;
        let receipt = OperationReceipt {
            operation: op,
            doctype: doc.doctype().as_str().to_owned(),
            name: doc.id().to_string(),
            version,
            effects,
            committed_at: inv.now,
        };
        let undeclared = self.undeclared_in(snap, &receipt);
        if self.strict_effects
            && let Some(first) = undeclared.first().and_then(|i| receipt.effects.get(*i))
        {
            return Err(AppError::new(
                ErrorClass::Internal,
                "document.internal_error",
                format!(
                    "A step reported an effect it did not declare (step {}, class {}).",
                    first.step, first.class
                ),
            )
            .detail("step", first.step.as_str())
            .detail("class", first.class.as_str()));
        }
        if !receipt.effects.is_empty() {
            let mut data = serde_json::to_value(&receipt).map_err(receipt_error)?;
            if let Json::Object(map) = &mut data
                && !undeclared.is_empty()
            {
                map.insert("undeclared".to_owned(), json!(undeclared));
            }
            let event = outbox::receipt(&*self.ids, inv, doc, version, data)?;
            work.tx
                .enqueue(&event)
                .await
                .map_err(|e| work.fail(e, StorageOp::Write))?;
        }
        work.finished.push(Finished {
            record: ReceiptRecord {
                receipt,
                undeclared,
            },
            depth: work.depth,
        });
        Ok(())
    }

    /// Turns what a phase-4 hook point reported into outbox events and the entries to merge into
    /// the caller's receipt. With `strict_effects`, an undeclared report is not recorded: the
    /// operation already committed, so it is a committed-step failure instead.
    pub(crate) fn announce(
        &self,
        snap: &Snapshot,
        inv: &Invocation,
        op: Operation,
        doc: &Document,
        collected: ReceiptCollector,
    ) -> AppResult<(Vec<OutboxEvent>, Vec<Announced>)> {
        if collected.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let version = doc
            .version()
            .ok_or_else(|| internal("a committed step ran for a document without a version"))?;
        let name = doc.id().to_string();
        let doctype = doc.doctype().as_str().to_owned();
        let (mut events, mut announced) = (Vec::new(), Vec::new());
        for effect in collected.finish()? {
            let probe = OperationReceipt {
                operation: op,
                doctype: doctype.clone(),
                name: name.clone(),
                version,
                effects: vec![effect.clone()],
                committed_at: inv.now,
            };
            let undeclared = !self.undeclared_in(snap, &probe).is_empty();
            if undeclared && self.strict_effects {
                self.record_failure(CommittedFailure {
                    point: format!("{}_committed", spot_app_api::lifecycle::op_name(op)),
                    doctype: doctype.clone(),
                    id: doc.id(),
                    error: internal("A committed step reported an effect it did not declare.")
                        .detail("step", effect.step.as_str())
                        .detail("class", effect.class.as_str()),
                    at: self.clock.now_utc(),
                });
                continue;
            }
            let effect = sanitised(effect);
            let data = json!({
                "operation": spot_app_api::lifecycle::op_name(op),
                "effect": serde_json::to_value(&effect).map_err(receipt_error)?,
                "undeclared": undeclared,
            });
            events.push(outbox::receipt_effect(&*self.ids, inv, doc, version, data)?);
            announced.push(Announced {
                doctype: doctype.clone(),
                name: name.clone(),
                version,
                operation: op,
                effect,
                undeclared,
            });
        }
        Ok((events, announced))
    }

    // -------------------------------------------------------------------------------------
    // Receipts returned by every lifecycle operation
    // -------------------------------------------------------------------------------------

    /// Creates a document, like `DocumentService::create`, and returns the operation's receipt.
    ///
    /// # Errors
    ///
    /// Those of `create`; `document.internal_error` if a step reported an effect it did not
    /// declare and `strict_effects` is on.
    pub async fn create_with_receipt(
        &self,
        inv: &Invocation,
        doc: Document,
    ) -> AppResult<Receipted<Document>> {
        let done = self.top(inv, super::ops::Req::Create(doc)).await?;
        receipted(done, Done::into_doc)
    }

    /// Updates a document, like `DocumentService::update`, and returns the operation's
    /// receipt.
    ///
    /// # Errors
    ///
    /// As [`create_with_receipt`](Self::create_with_receipt).
    pub async fn update_with_receipt(
        &self,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> AppResult<Receipted<Document>> {
        let req = super::ops::Req::Update {
            doctype: doctype.to_owned(),
            id,
            patch,
            expected,
        };
        receipted(self.top(inv, req).await?, Done::into_doc)
    }

    /// Posts a draft, like `DocumentService::post`, and returns the operation's receipt: the
    /// facts a process stores to reverse the post later.
    ///
    /// # Errors
    ///
    /// As [`create_with_receipt`](Self::create_with_receipt).
    pub async fn post_with_receipt(
        &self,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        expected: Version,
    ) -> AppResult<Receipted<Document>> {
        let req = super::ops::Req::Post {
            doctype: doctype.to_owned(),
            id,
            expected,
        };
        receipted(self.top(inv, req).await?, Done::into_doc)
    }

    /// Cancels a posted document, like `DocumentService::cancel`, and returns the operation's
    /// receipt.
    ///
    /// # Errors
    ///
    /// As [`create_with_receipt`](Self::create_with_receipt).
    pub async fn cancel_with_receipt(
        &self,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        expected: Version,
    ) -> AppResult<Receipted<Document>> {
        let req = super::ops::Req::Cancel {
            doctype: doctype.to_owned(),
            id,
            expected,
        };
        receipted(self.top(inv, req).await?, Done::into_doc)
    }

    /// Cancels a posted document and creates its amended draft, like
    /// `DocumentService::correct`. `receipt` is the cancel of the original; `nested[0]` is the
    /// creation of the draft (followed by anything their steps wrote).
    ///
    /// # Errors
    ///
    /// As [`create_with_receipt`](Self::create_with_receipt).
    pub async fn correct_with_receipt(
        &self,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        expected: Version,
    ) -> AppResult<Receipted<Document>> {
        let req = super::ops::Req::Correct {
            doctype: doctype.to_owned(),
            id,
            expected,
        };
        receipted(self.top(inv, req).await?, Done::into_doc)
    }

    /// Deletes a draft, like `DocumentService::delete`, and returns the operation's receipt.
    ///
    /// # Errors
    ///
    /// As [`create_with_receipt`](Self::create_with_receipt).
    pub async fn delete_with_receipt(
        &self,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        expected: Version,
    ) -> AppResult<Receipted<()>> {
        let req = super::ops::Req::Delete {
            doctype: doctype.to_owned(),
            id,
            expected,
        };
        receipted(self.top(inv, req).await?, Done::into_unit)
    }

    /// Renames a document, like `DocumentService::rename`, and returns the operation's
    /// receipt.
    ///
    /// # Errors
    ///
    /// As [`create_with_receipt`](Self::create_with_receipt).
    pub async fn rename_with_receipt(
        &self,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        new_name: &str,
        expected: Version,
    ) -> AppResult<Receipted<Document>> {
        let req = super::ops::Req::Rename {
            doctype: doctype.to_owned(),
            id,
            new_name: new_name.to_owned(),
            expected,
        };
        receipted(self.top(inv, req).await?, Done::into_doc)
    }

    // -------------------------------------------------------------------------------------
    // Reading receipts back
    // -------------------------------------------------------------------------------------

    /// One receipt of a document, as it stands now: with the external effects announced since
    /// the commit folded in. `name` is the record's name as the receipt gives it (its id as
    /// text).
    ///
    /// Allowed to whoever may read the document (the read decision is the same as `load`'s). A
    /// deleted document has no one to decide for, so its receipt is not found.
    ///
    /// The receipt is rebuilt from the outbox by reading its feed: the engine has no index of its
    /// own. That is fine for an audit view and a process step, not for a hot path; the platform's
    /// outbox projection (WP-PLT-06) is the place for an index.
    ///
    /// # Errors
    ///
    /// `document.not_found` when the document does not exist or has no such operation at that
    /// version; `permission.denied` when the caller may not read the document; the mapped
    /// storage error otherwise.
    pub async fn get_receipt(
        &self,
        inv: &Invocation,
        doctype: &str,
        name: &str,
        version: Version,
        operation: Operation,
    ) -> AppResult<ReceiptRecord> {
        self.document_receipts(inv, doctype, name)
            .await?
            .into_iter()
            .find(|r| r.matches(doctype, name, version, operation))
            .ok_or_else(not_found)
    }

    /// Every receipt of a document, oldest version first: the document's receipts for its audit
    /// view. Operations whose steps reported nothing are listed with an empty receipt.
    ///
    /// Permission and cost are those of [`get_receipt`](Self::get_receipt).
    ///
    /// # Errors
    ///
    /// As [`get_receipt`](Self::get_receipt), without the missing operation.
    pub async fn document_receipts(
        &self,
        inv: &Invocation,
        doctype: &str,
        name: &str,
    ) -> AppResult<Vec<ReceiptRecord>> {
        let id = outbox::parse_id(name).ok_or_else(not_found)?;
        let snap = self.snapshot();
        let doc = self
            .read_document(&snap, doctype, id)
            .await?
            .ok_or_else(not_found)?;
        // The decision is the one a read of the document gets: whoever may not read it may not
        // read what its operations did either.
        self.authorize(inv, Action::Read, &doc).await?;
        let mut folded: BTreeMap<(u64, usize), ReceiptRecord> = BTreeMap::new();
        let mut after = FeedPosition::START;
        loop {
            let page = self
                .backend
                .read_outbox(after, PAGE)
                .await
                .map_err(|e| map_storage_error(&e, StorageOp::Other))?;
            let Some(last) = page.last() else { break };
            after = last.position;
            for record in &page {
                fold(&mut folded, record, doctype, id)?;
            }
        }
        Ok(folded.into_values().collect())
    }
}

/// The index of an operation in [`EventOperation::ALL`], for ordering.
fn op_index(op: Operation) -> usize {
    EventOperation::ALL
        .iter()
        .position(|o| *o == op)
        .unwrap_or(usize::MAX)
}

fn operation_of_topic(topic: &str) -> Option<Operation> {
    EventOperation::ALL
        .into_iter()
        .find(|o| o.topic_name() == topic)
}

fn operation_of_name(name: &str) -> Option<Operation> {
    EventOperation::ALL
        .into_iter()
        .find(|o| spot_app_api::lifecycle::op_name(*o) == name)
}

/// Folds one outbox record about `doctype`/`id` into the receipts being rebuilt.
fn fold(
    folded: &mut BTreeMap<(u64, usize), ReceiptRecord>,
    record: &OutboxRecord,
    doctype: &str,
    id: RecordId,
) -> AppResult<()> {
    let event = &record.event;
    if event.subject.entity.as_str() != doctype || event.subject.id != id {
        return Ok(());
    }
    let Some(version) = event.subject_version else {
        return Ok(());
    };
    let topic = event.topic.as_str();
    if let Some(op) = operation_of_topic(topic) {
        // The lifecycle event is the receipt of an operation that reported nothing.
        let payload = PayloadDocument::from_outbox_event(event).map_err(receipt_error)?;
        folded.entry((version.0, op_index(op))).or_insert_with(|| {
            ReceiptRecord::empty(
                op,
                doctype,
                id.to_string(),
                version,
                payload.meta.occurred_at,
            )
        });
        return Ok(());
    }
    if topic == TOPIC_RECEIPT {
        let payload = PayloadDocument::from_outbox_event(event).map_err(receipt_error)?;
        let mut data = payload.data;
        let undeclared = match data.as_object_mut().and_then(|m| m.remove("undeclared")) {
            Some(list) => serde_json::from_value::<Vec<usize>>(list).map_err(receipt_error)?,
            None => Vec::new(),
        };
        let receipt: OperationReceipt = serde_json::from_value(data).map_err(receipt_error)?;
        let key = (receipt.version.0, op_index(receipt.operation));
        match folded.get_mut(&key) {
            Some(existing) => {
                // The lifecycle event came first and held an empty receipt; the receipt event
                // has the effects.
                existing.receipt = receipt;
                existing.undeclared = undeclared;
            }
            None => {
                folded.insert(
                    key,
                    ReceiptRecord {
                        receipt,
                        undeclared,
                    },
                );
            }
        }
        return Ok(());
    }
    if topic == TOPIC_RECEIPT_EFFECT {
        let payload = PayloadDocument::from_outbox_event(event).map_err(receipt_error)?;
        let op = payload
            .data
            .get("operation")
            .and_then(Json::as_str)
            .and_then(operation_of_name)
            .ok_or_else(|| receipt_error("an announcement without an operation"))?;
        let effect: EffectReceipt = serde_json::from_value(
            payload
                .data
                .get("effect")
                .cloned()
                .ok_or_else(|| receipt_error("an announcement without an effect"))?,
        )
        .map_err(receipt_error)?;
        let undeclared = payload
            .data
            .get("undeclared")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        folded
            .entry((version.0, op_index(op)))
            .or_insert_with(|| {
                ReceiptRecord::empty(
                    op,
                    doctype,
                    id.to_string(),
                    version,
                    payload.meta.occurred_at,
                )
            })
            .apply(effect, undeclared);
    }
    Ok(())
}
