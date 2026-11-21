//! Many creates of one actor in one transaction (the bulk write path of the import engine).
//!
//! [`Engine::create_batch`] gives every document the lifecycle a lone
//! [`create`](spot_app_api::context::DocumentService::create) gives it: the operation gate and
//! the field and link checks, defaults, naming, validation, the steps and participants, the
//! storage write, the lifecycle, receipt and audit events, the journal's state events, the
//! command ledger and the committed steps. What is shared is the cost around them:
//!
//! - one transaction (one commit, one WAL flush) for the whole batch;
//! - the principal is resolved once, not once per document and again for the masking of each
//!   result.
//!
//! # Per-document semantics are kept
//!
//! Each document runs inside its own savepoint. A document that is refused (validation, a
//! permission, a unique key, a step) rolls back to its savepoint and is reported as that
//! document's error; the others in the batch still commit. A document whose command id the
//! ledger already holds is answered from the ledger as a replay (or is
//! `document.command_conflict` when the stored request differs), exactly like a lone create.
//!
//! # What is not the same as N lone calls
//!
//! - The principal is read once per batch: a permission change made while a batch runs takes
//!   effect from the next batch (a lone call reads it per call). A batch is one request.
//! - A failure that ends the transaction itself (storage down, a deferred constraint at commit)
//!   cannot be pinned on one document. A retryable storage error reruns the whole batch like a
//!   lone call; any other failure falls back to creating each document alone, so every
//!   document still gets its own verdict.
//! - Batches whose invocations differ in actor, or repeat a command id, are not batched: each
//!   document runs alone.

use std::sync::Mutex;

use spot_app_api::context::{Invocation, Outcome};
use spot_app_api::error::StorageOp;
use spot_app_api::{AppError, AppResult, Document, ErrorClass, Message};
use spot_permissions::Principal;
use spot_storage_contract::{CommandRecord, Isolation, StorageBackend, Transaction, TxOptions};
use tracing::{Instrument, Span};

use super::{Done, EngineSteps, Failed, Req, Ret, TxOutbox, batch_of, is_step_stale, label};
use crate::engine::ledger::command_conflict;
use crate::engine::receipt::{Finished, merge_announced};
use crate::engine::work::{CommittedItem, Trail, Work};
use crate::engine::{Engine, Snapshot};

/// What one document of a batch came to inside the transaction.
enum Slot {
    /// The ledger held this command id: nothing ran.
    Replay(CommandRecord),
    /// The document was refused; its work was rolled back to its savepoint.
    Refused(AppError, Trail),
    /// The document was created: its result and what to do after the commit.
    Ran(Ran),
}

/// A created document's result and the post-commit work it carries.
struct Ran {
    value: Ret,
    messages: Vec<Message>,
    committed: Vec<CommittedItem>,
    receipts: Vec<Finished>,
}

impl<B: StorageBackend> Engine<B> {
    /// Creates many documents of one actor in one transaction and answers one result per
    /// document, in order. See the [module docs](self) for what is and is not the same as one
    /// `create` per document.
    ///
    /// Every invocation must carry the same actor and a distinct command id (or none); a batch
    /// that does not is run document by document.
    pub async fn create_batch(
        &self,
        items: Vec<(Invocation, Document)>,
    ) -> Vec<AppResult<Outcome<Document>>> {
        if items.len() < 2 || !batchable(&items) {
            return self.create_each(items).await;
        }
        let span = tracing::info_span!("doc.op", doctype = "batch", operation = "create_batch");
        async move {
            let snap = self.snapshot();
            let principal = self.authorizer.principal_for(&items[0].0).await;
            let op_label = label(items[0].1.doctype().as_str(), "create");
            let attempted = self
                .retrying(&op_label, || {
                    self.attempt_batch(&snap, &principal, &items, &op_label)
                })
                .await;
            let slots = match attempted {
                Ok(slots) => slots,
                // The storage itself failed: say so for every document; a rerun one by one
                // would only fail the same way, slowly.
                Err(error)
                    if matches!(error.class, ErrorClass::Unavailable | ErrorClass::Internal) =>
                {
                    return items.iter().map(|_| Err(error.clone())).collect();
                }
                // A refusal the batch cannot pin on one document (a deferred check at the
                // commit): every document gets its own verdict.
                Err(_) => return self.create_each(items).await,
            };
            let mut results = Vec::with_capacity(slots.len());
            for ((inv, doc), slot) in items.iter().zip(slots) {
                results.push(self.finish_slot(&snap, &principal, inv, doc, slot).await);
            }
            results
        }
        .instrument(span)
        .await
    }

    /// One `create` per document, in order (the path for what cannot be batched).
    async fn create_each(
        &self,
        items: Vec<(Invocation, Document)>,
    ) -> Vec<AppResult<Outcome<Document>>> {
        let mut results = Vec::with_capacity(items.len());
        for (inv, doc) in items {
            results.push(
                self.create_with_receipt(&inv, doc)
                    .await
                    .map(|done| done.outcome),
            );
        }
        results
    }

    /// One attempt: open the transaction, run every document in its savepoint, commit.
    async fn attempt_batch(
        &self,
        snap: &Snapshot,
        principal: &Principal,
        items: &[(Invocation, Document)],
        op_label: &spot_app_api::prelude::Name,
    ) -> Result<Vec<Slot>, Failed> {
        let options = TxOptions::new(op_label.clone(), Isolation::ReadCommitted);
        let tx = self
            .backend
            .begin(snap.catalog.clone(), options)
            .await
            .map_err(|e| Failed::Storage(e, StorageOp::Other))?;
        let mut work = Work::new(tx);
        if let Some((inv, _)) = items.first() {
            work.pin_principal(inv, principal);
        }
        let mut slots = Vec::with_capacity(items.len());
        for (inv, doc) in items {
            let req = Req::Create(doc.clone());
            let ledger = self.ledger_call(inv, &req, None);
            if let Some(call) = &ledger {
                match work.tx.command_get(&call.principal, &call.command_id).await {
                    Ok(None) => {}
                    Ok(Some(record)) => {
                        slots.push(if record.fingerprint == call.fingerprint {
                            Slot::Replay(record)
                        } else {
                            Slot::Refused(command_conflict(), Trail::default())
                        });
                        continue;
                    }
                    Err(e) => {
                        let _ = work.tx.rollback().await;
                        return Err(Failed::Storage(e, StorageOp::Other));
                    }
                }
            }
            let savepoint = match work.tx.savepoint().await {
                Ok(savepoint) => savepoint,
                Err(e) => {
                    let _ = work.tx.rollback().await;
                    return Err(Failed::Storage(e, StorageOp::Other));
                }
            };
            let mark = work.mark();
            let from = (
                work.messages.len(),
                work.committed.len(),
                work.finished.len(),
            );
            let result = self
                .run_batch_item(
                    snap,
                    &mut work,
                    inv,
                    doc.clone(),
                    principal,
                    &req,
                    ledger.as_ref(),
                    from.1,
                )
                .await;
            match (result, work.poisoned.take()) {
                (Ok(value), None) => {
                    if let Err(e) = work.tx.release(savepoint).await {
                        let _ = work.tx.rollback().await;
                        return Err(Failed::Storage(e, StorageOp::Other));
                    }
                    slots.push(Slot::Ran(Ran {
                        value,
                        messages: work.messages.split_off(from.0),
                        committed: work.committed.split_off(from.1),
                        receipts: work.finished.split_off(from.2),
                    }));
                }
                // The transaction cannot go on after this storage error.
                (outcome, Some((e, during))) => {
                    let _ = work.tx.rollback().await;
                    return Err(match outcome {
                        Err(app) if !e.kind.is_retryable() => Failed::App(app),
                        _ => Failed::Storage(e, during),
                    });
                }
                (Err(error), None) => {
                    if is_step_stale(&error) {
                        let _ = work.tx.rollback().await;
                        return Err(Failed::Conflict(error));
                    }
                    if let Err(e) = work.tx.rollback_to(savepoint).await {
                        let _ = work.tx.rollback().await;
                        return Err(Failed::Storage(e, StorageOp::Other));
                    }
                    work.truncate(&mark);
                    work.trail.cause = work.cause.clone();
                    let trail = std::mem::take(&mut work.trail);
                    slots.push(Slot::Refused(error, trail));
                }
            }
        }
        work.tx.commit().await.map_err(|e| {
            match super::links::commit_violation(snap, None, &e) {
                Some(linked) => Failed::App(linked),
                None => Failed::Storage(e, StorageOp::Write),
            }
        })?;
        Ok(slots)
    }

    /// One document inside its savepoint: the create, the journal's state events, the durable
    /// delivery's job and the ledger row, in the order a lone create writes them.
    #[allow(clippy::too_many_arguments)]
    async fn run_batch_item(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        doc: Document,
        principal: &Principal,
        req: &Req,
        ledger: Option<&crate::engine::ledger::LedgerCall>,
        committed_from: usize,
    ) -> AppResult<Ret> {
        let messages_from = work.messages.len();
        let created = self.do_create_as(snap, work, inv, doc, principal).await?;
        let value = Ret::Doc(created);
        let planned: Vec<CommittedItem> = work.committed[committed_from..]
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
                return Err(match storage {
                    Some(e) => work.fail(e, StorageOp::Write),
                    None => error,
                });
            }
        }
        self.flush_capture(snap, work, inv).await?;
        if let Some(call) = ledger {
            let record = self.command_record(call, req, &value, &work.messages[messages_from..])?;
            work.tx
                .command_put(record)
                .await
                .map_err(|e| work.fail(e, StorageOp::Write))?;
        }
        Ok(value)
    }

    /// After the commit: the answer for one document.
    async fn finish_slot(
        &self,
        snap: &Snapshot,
        principal: &Principal,
        inv: &Invocation,
        doc: &Document,
        slot: Slot,
    ) -> AppResult<Outcome<Document>> {
        match slot {
            Slot::Replay(record) => self
                .replayed(inv, &Req::Create(doc.clone()), &record)
                .await?
                .into_doc(),
            Slot::Refused(error, trail) => {
                // The transaction has rolled back this document's work: tell the sink.
                self.report_refusal(inv, &Req::Create(doc.clone()), &trail, &error)
                    .await;
                Err(error)
            }
            Slot::Ran(Ran {
                value,
                mut messages,
                committed,
                mut receipts,
            }) => {
                let value = self.mask_result(principal, value).await?;
                let batch = batch_of(inv, &committed);
                let steps = EngineSteps {
                    engine: self,
                    snap,
                    announced: Mutex::new(Vec::new()),
                };
                let delivered = self
                    .delivery
                    .deliver(&steps, batch)
                    .instrument(tracing::info_span!("doc.phase", phase = "committed"))
                    .await;
                messages.extend(delivered);
                let announced = steps
                    .announced
                    .into_inner()
                    .unwrap_or_else(|p| p.into_inner());
                merge_announced(&mut receipts, announced);
                Span::current().record("outcome", "ok");
                Done {
                    value,
                    messages,
                    receipts,
                }
                .into_doc()
            }
        }
    }
}

/// Whether one transaction may hold these creates: one actor, and no command id twice.
fn batchable(items: &[(Invocation, Document)]) -> bool {
    let actor = items[0].0.actor.principal_text();
    let mut ids = std::collections::HashSet::new();
    items.iter().all(|(inv, _)| {
        inv.actor.principal_text() == actor
            && inv
                .command_id
                .as_ref()
                .is_none_or(|id| ids.insert(id.as_str().to_owned()))
    })
}
