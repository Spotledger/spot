//! Operation receipts (WP-DOC-12.T1; ADR-080 section 6.4): the effects steps report are collected
//! into the receipt of the operation, persisted with it, and readable later.
//!
//! The fixture is a small "books" app on the posted `sales_invoice`: posting writes a store row
//! (a `customer` document stands in for a movement store, so the write is a real, rollbackable
//! row) and reports it; cancelling writes the reversing row; an external mail effect is announced
//! after the commit.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{meta, n, now, valid_invoice};
use spot_app_api::context::{Actor, HookContext, Invocation};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Version};
use spot_app_api::{
    ActionRef, AppError, AppResult, BoxFuture, Controller, Document, EffectClassName, EffectDecl,
    EffectKind, EffectReceipt, ErrorClass, Hex32, HookPoint, OpaqueRef, Operation, Phase,
    ReceiptState, Reversal, StepEffects, StoreName, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{
    AllowAllAuthorizer, Authorizer, CommittedBatch, CommittedDelivery, CommittedSteps, Engine,
    MAX_EFFECTS_PER_RECEIPT, QueryPlan, ReceiptRecord, TOPIC_RECEIPT, TOPIC_RECEIPT_EFFECT, V7Ids,
    WriteChange, rows_digest,
};
use spot_permissions::{Action, Decision, DocFacts, Principal};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, FeedPosition, OutboxEvent};
use spot_storage_fake::FakeBackend;

/// Text that must never appear in a receipt: it is in a field of the document and in the store
/// row the step writes.
const CANARY: &str = "CANARY-9f3a71";

const BOOKS: &str = "books";
const CLASS: &str = "balance_movement";
const MAIL: &str = "mail";

fn class(name: &str) -> EffectClassName {
    EffectClassName::new(name).unwrap()
}

fn step(name: &str) -> StepName {
    StepName::new(&format!("{BOOKS}:{name}")).unwrap()
}

fn after(op: Operation) -> HookPoint {
    HookPoint::of(Phase::After, Target::Op(op))
}

fn committed(op: Operation) -> HookPoint {
    HookPoint::of(Phase::Committed, Target::Op(op))
}

/// What the books app does differently from one test to the next.
#[derive(Default)]
struct Knobs {
    /// A step after the booking step fails, so phase 3 ends in an error.
    fail_after_booking: AtomicBool,
    /// The booking step also reports an effect of a class it did not declare.
    rogue: AtomicBool,
    /// The booking step reports more effects than a receipt may hold.
    flood: AtomicBool,
    /// The states the mail step reports, one per run; `Delivered` when the script is empty.
    mail_states: Mutex<VecDeque<ReceiptState>>,
}

struct Books {
    knobs: Arc<Knobs>,
}

impl Books {
    fn internal() -> EffectDecl {
        EffectDecl::new(
            EffectKind::Internal,
            class(CLASS),
            vec![StoreName::new("movements").unwrap()],
            Reversal::ReversibleBy(Operation::Cancel),
        )
    }
}

fn digest_of(id: &str, version: u64) -> Hex32 {
    rows_digest(&[(id, Version(version))]).unwrap()
}

fn reported(step_name: &str, row: &str, state: ReceiptState) -> EffectReceipt {
    EffectReceipt {
        step: step(step_name),
        class: class(CLASS),
        kind: EffectKind::Internal,
        store_refs: vec![OpaqueRef::new(row).unwrap()],
        count: 1,
        digest: digest_of(row, 1),
        state,
    }
}

impl Controller for Books {
    fn doctype(&self) -> &str {
        "sales_invoice"
    }

    fn steps(&self) -> Vec<StepDecl> {
        let decl = |name: &str, point: HookPoint, priority: i32| StepDecl {
            name: step(name),
            point,
            priority,
            budget: Duration::from_secs(5),
        };
        vec![
            decl("book_post", after(Operation::Post), 100),
            decl("fail_post", after(Operation::Post), 200),
            decl("book_cancel", after(Operation::Cancel), 100),
            decl("notify_post", committed(Operation::Post), 100),
        ]
    }

    fn step_effects(&self) -> Vec<StepEffects> {
        vec![
            StepEffects::new(step("book_post"), vec![Self::internal()]),
            StepEffects::new(step("book_cancel"), vec![Self::internal()]),
            StepEffects::new(
                step("notify_post"),
                vec![EffectDecl::new(
                    EffectKind::External,
                    class(MAIL),
                    vec![StoreName::new("mail_out").unwrap()],
                    Reversal::CompensableBy(ActionRef::new("books:send_correction").unwrap()),
                )],
            ),
        ]
    }

    fn run_step<'a>(
        &'a self,
        name: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            match name.as_str() {
                "books:book_post" | "books:book_cancel" => {
                    // The store row: a real document, written in the business transaction, with
                    // the canary in a field. Only its id may reach the receipt.
                    let mut row = Document::new(n("customer"), RecordId::new_v7());
                    row.set(n("name"), CANARY);
                    let row = ctx.create(row).await?;
                    let short = name.as_str().trim_start_matches("books:").to_owned();
                    ctx.report_effect(reported(
                        &short,
                        &row.id().to_string(),
                        ReceiptState::Applied,
                    ));
                    if self.knobs.rogue.load(Ordering::SeqCst) {
                        ctx.report_effect(EffectReceipt {
                            class: class("secret_side_channel"),
                            ..reported(&short, "rogue-row", ReceiptState::Applied)
                        });
                    }
                    if self.knobs.flood.load(Ordering::SeqCst) {
                        for i in 0..=MAX_EFFECTS_PER_RECEIPT {
                            ctx.report_effect(reported(
                                &short,
                                &format!("flood-{i}"),
                                ReceiptState::Applied,
                            ));
                        }
                    }
                }
                "books:fail_post" => {
                    if self.knobs.fail_after_booking.load(Ordering::SeqCst) {
                        return Err(AppError::new(
                            ErrorClass::InvalidState,
                            "document.invalid_state",
                            "stop",
                        ));
                    }
                }
                "books:notify_post" => {
                    let state = self
                        .knobs
                        .mail_states
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(ReceiptState::Delivered);
                    let message = "msg-1";
                    ctx.report_effect(EffectReceipt {
                        step: step("notify_post"),
                        class: class(MAIL),
                        kind: EffectKind::External,
                        store_refs: vec![OpaqueRef::new(message).unwrap()],
                        count: 1,
                        digest: digest_of(message, 1),
                        state,
                    });
                }
                _ => {}
            }
            Ok(())
        })
    }
}

/// Delivers every committed step `times` times: what a redelivering outbox path does.
struct Repeat(u32);

impl CommittedDelivery for Repeat {
    fn deliver<'a>(
        &'a self,
        steps: &'a dyn CommittedSteps,
        batch: CommittedBatch,
    ) -> BoxFuture<'a, Vec<spot_app_api::Message>> {
        Box::pin(async move {
            let mut messages = Vec::new();
            for step in &batch.steps {
                for _ in 0..self.0 {
                    messages.extend(steps.run(&batch.invocation, step.clone()).await);
                }
            }
            messages
        })
    }
}

struct Fixture {
    engine: Engine<FakeBackend>,
    knobs: Arc<Knobs>,
}

async fn fixture_with(
    strict: bool,
    delivery: Option<Arc<dyn CommittedDelivery>>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> Fixture {
    let knobs = Arc::new(Knobs::default());
    let mut builder = RegistryBuilder::new();
    builder.add_controller(
        BOOKS,
        Arc::new(Books {
            knobs: Arc::clone(&knobs),
        }),
    );
    let mut engine = Engine::new(
        FakeBackend::new(),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_strict_effects(strict);
    if let Some(delivery) = delivery {
        engine = engine.with_delivery(delivery);
    }
    if let Some(authorizer) = authorizer {
        engine = engine.with_authorizer(authorizer);
    }
    engine.prepare().await.unwrap();
    Fixture { engine, knobs }
}

async fn fixture() -> Fixture {
    fixture_with(true, None, None).await
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("receipts.test").unwrap(),
        actor: Actor::System {
            component: n("receipt_tests"),
        },
        command_id: CommandId::new("cmd-12").ok(),
        language: None,
        now: now(),
    }
}

/// A valid invoice for a customer that exists, with the canary in a field.
async fn invoice(f: &Fixture) -> Document {
    let mut customer = Document::new(n("customer"), RecordId::new_v7());
    customer.set(n("name"), "Ada");
    let customer = f
        .engine
        .create_with_receipt(&inv(), customer)
        .await
        .unwrap()
        .outcome
        .value;
    let mut doc = valid_invoice();
    doc.set(n("customer"), customer.id());
    doc.set(n("remarks"), CANARY);
    doc
}

async fn outbox(engine: &Engine<FakeBackend>) -> Vec<OutboxEvent> {
    engine
        .backend()
        .read_outbox(FeedPosition::START, 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.event)
        .collect()
}

async fn topic_count(engine: &Engine<FakeBackend>, topic: &str) -> usize {
    outbox(engine)
        .await
        .iter()
        .filter(|e| e.topic.as_str() == topic)
        .count()
}

/// A draft invoice, created.
async fn draft(f: &Fixture) -> Document {
    f.engine
        .create_with_receipt(&inv(), invoice(f).await)
        .await
        .unwrap()
        .outcome
        .value
}

fn name_of(doc: &Document) -> String {
    doc.id().to_string()
}

fn version(doc: &Document) -> Version {
    doc.version().unwrap()
}

fn external(record: &ReceiptRecord) -> Vec<&EffectReceipt> {
    record
        .receipt
        .effects
        .iter()
        .filter(|e| e.kind == EffectKind::External)
        .collect()
}

#[tokio::test]
async fn post_cancel_and_correct_return_receipts_of_the_right_shape() {
    let f = fixture().await;
    let created = f
        .engine
        .create_with_receipt(&inv(), invoice(&f).await)
        .await
        .unwrap();
    // An operation whose steps report nothing has an empty receipt: valid.
    assert_eq!(created.receipt.receipt.operation, Operation::Create);
    assert!(created.receipt.receipt.effects.is_empty());
    let doc = created.outcome.value;

    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    let post = &posted.receipt.receipt;
    assert_eq!(post.operation, Operation::Post);
    assert_eq!(post.doctype, "sales_invoice");
    assert_eq!(post.name, name_of(&doc));
    assert_eq!(post.version, version(&posted.outcome.value));
    assert_eq!(post.committed_at, now());
    // One internal effect from phase 3, then the external one announced after the commit.
    assert_eq!(post.effects.len(), 2);
    let movement = &post.effects[0];
    assert_eq!(movement.step, step("book_post"));
    assert_eq!(movement.class, class(CLASS));
    assert_eq!(movement.kind, EffectKind::Internal);
    assert_eq!(movement.count, 1);
    assert_eq!(movement.store_refs.len(), 1);
    assert_eq!(movement.state, ReceiptState::Applied);
    assert_eq!(
        movement.digest,
        digest_of(movement.store_refs[0].as_str(), 1)
    );
    let mail = &post.effects[1];
    assert_eq!(mail.kind, EffectKind::External);
    assert_eq!(mail.state, ReceiptState::Delivered);
    assert!(posted.receipt.undeclared.is_empty());
    // The store row the step wrote has its own (empty) receipt, as its own operation.
    assert_eq!(posted.nested.len(), 1);
    assert_eq!(posted.nested[0].receipt.operation, Operation::Create);
    assert_eq!(posted.nested[0].receipt.doctype, "customer");

    let cancelled = f
        .engine
        .cancel_with_receipt(
            &inv(),
            "sales_invoice",
            doc.id(),
            version(&posted.outcome.value),
        )
        .await
        .unwrap();
    let cancel = &cancelled.receipt.receipt;
    assert_eq!(cancel.operation, Operation::Cancel);
    assert_eq!(cancel.version, version(&cancelled.outcome.value));
    assert_eq!(cancel.effects.len(), 1);
    assert_eq!(cancel.effects[0].step, step("book_cancel"));
    assert_ne!(
        cancel.effects[0].store_refs, movement.store_refs,
        "the reversal is a row of its own"
    );

    // Correct: the receipt is the cancel of the original; the amended draft's creation follows
    // among the nested ones.
    let again = draft(&f).await;
    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", again.id(), version(&again))
        .await
        .unwrap();
    let corrected = f
        .engine
        .correct_with_receipt(
            &inv(),
            "sales_invoice",
            again.id(),
            version(&posted.outcome.value),
        )
        .await
        .unwrap();
    assert_eq!(corrected.receipt.receipt.operation, Operation::Cancel);
    assert_eq!(corrected.receipt.receipt.name, name_of(&again));
    assert_eq!(corrected.receipt.receipt.effects.len(), 1);
    let amended = corrected.nested.last().unwrap();
    assert_eq!(amended.receipt.operation, Operation::Create);
    assert_eq!(amended.receipt.name, name_of(&corrected.outcome.value));
}

#[tokio::test]
async fn the_old_service_methods_still_work_and_drop_the_receipt() {
    use spot_app_api::context::DocumentService;
    let f = fixture().await;
    let doc = draft(&f).await;
    let posted = f
        .engine
        .post(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    assert_eq!(posted.value.state(), Some(spot_app_api::DocState::Posted));
    // The receipt was persisted all the same.
    assert_eq!(topic_count(&f.engine, TOPIC_RECEIPT).await, 1);
}

#[tokio::test]
async fn receipts_are_retrievable_later_in_the_audit_view() {
    let f = fixture().await;
    let doc = draft(&f).await;
    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    let cancelled = f
        .engine
        .cancel_with_receipt(
            &inv(),
            "sales_invoice",
            doc.id(),
            version(&posted.outcome.value),
        )
        .await
        .unwrap();

    let name = name_of(&doc);
    let read = f
        .engine
        .get_receipt(
            &inv(),
            "sales_invoice",
            &name,
            version(&posted.outcome.value),
            Operation::Post,
        )
        .await
        .unwrap();
    // What was returned is what is stored, phase-4 entry included.
    assert_eq!(read, posted.receipt);

    let all = f
        .engine
        .document_receipts(&inv(), "sales_invoice", &name)
        .await
        .unwrap();
    let ops: Vec<Operation> = all.iter().map(|r| r.receipt.operation).collect();
    assert_eq!(
        ops,
        [Operation::Create, Operation::Post, Operation::Cancel],
        "oldest first, with the operation that reported nothing listed too"
    );
    assert!(all[0].receipt.effects.is_empty());
    assert_eq!(all[2], cancelled.receipt);

    // No such operation at that version.
    let missing = f
        .engine
        .get_receipt(&inv(), "sales_invoice", &name, Version(99), Operation::Post)
        .await
        .unwrap_err();
    assert_eq!(missing.code, "document.not_found");
    let missing = f
        .engine
        .get_receipt(
            &inv(),
            "sales_invoice",
            "not-an-id",
            version(&doc),
            Operation::Post,
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, "document.not_found");
}

/// A caller that may not read the document may not read its receipts.
struct DenyReads(AllowAllAuthorizer);

impl Authorizer for DenyReads {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        self.0.principal_for(inv)
    }
    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> BoxFuture<'a, Decision> {
        self.0.decide(principal, action, facts)
    }
    fn authorize<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        if action == Action::Read {
            return Box::pin(async {
                Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    "permission.denied",
                    "no",
                ))
            });
        }
        self.0.authorize(principal, action, doc)
    }
    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        self.0.authorize_write(principal, action, doc, changes)
    }
    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        def: &'a spot_app_api::DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.0.readable(principal, doc, def)
    }
    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a spot_app_api::prelude::Query,
    ) -> BoxFuture<'a, AppResult<Option<QueryPlan>>> {
        self.0.readable_query(principal, query)
    }
}

#[tokio::test]
async fn a_receipt_is_readable_only_by_those_who_can_read_the_document() {
    let f = fixture_with(true, None, Some(Arc::new(DenyReads(AllowAllAuthorizer)))).await;
    let doc = draft(&f).await;
    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    let denied = f
        .engine
        .get_receipt(
            &inv(),
            "sales_invoice",
            &name_of(&doc),
            version(&posted.outcome.value),
            Operation::Post,
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, "permission.denied");
    let denied = f
        .engine
        .document_receipts(&inv(), "sales_invoice", &name_of(&doc))
        .await
        .unwrap_err();
    assert_eq!(denied.code, "permission.denied");
}

#[tokio::test]
async fn a_receipt_never_holds_a_field_value() {
    let f = fixture().await;
    let doc = draft(&f).await;
    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    let cancelled = f
        .engine
        .cancel_with_receipt(
            &inv(),
            "sales_invoice",
            doc.id(),
            version(&posted.outcome.value),
        )
        .await
        .unwrap();
    // The canary is really in the document and in a store row ...
    assert_eq!(
        posted.outcome.value.get("remarks"),
        &spot_app_api::prelude::Value::Text(CANARY.to_owned())
    );
    let stored = outbox(&f.engine).await;
    assert!(
        stored
            .iter()
            .filter(|e| e.topic.as_str() == "doc_created" && e.subject.entity.as_str() == "customer")
            .count()
            >= 2,
        "the store rows exist"
    );
    // ... and nowhere in a receipt: not in what is returned, not in what is stored.
    for record in std::iter::once(&posted.receipt)
        .chain(&posted.nested)
        .chain(std::iter::once(&cancelled.receipt))
    {
        let wire = serde_json::to_string(&record.receipt).unwrap();
        assert!(!wire.contains(CANARY), "{wire}");
    }
    for event in stored
        .iter()
        .filter(|e| e.topic.as_str() == TOPIC_RECEIPT || e.topic.as_str() == TOPIC_RECEIPT_EFFECT)
    {
        assert!(!event.payload.contains(CANARY), "{}", event.payload);
    }
    let all = f
        .engine
        .document_receipts(&inv(), "sales_invoice", &name_of(&doc))
        .await
        .unwrap();
    assert!(!format!("{all:?}").contains(CANARY));
}

#[tokio::test]
async fn a_failure_in_phase_three_leaves_no_receipt_and_no_effect() {
    let f = fixture().await;
    let doc = draft(&f).await;
    let before = outbox(&f.engine).await.len();
    f.knobs.fail_after_booking.store(true, Ordering::SeqCst);
    let error = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap_err();
    assert_eq!(error.code, "document.invalid_state");
    // Nothing was written: no receipt event, no store row, no lifecycle event, the document is
    // still a draft, and nothing was announced after a commit that never happened.
    assert_eq!(outbox(&f.engine).await.len(), before);
    assert_eq!(topic_count(&f.engine, TOPIC_RECEIPT).await, 0);
    assert_eq!(topic_count(&f.engine, TOPIC_RECEIPT_EFFECT).await, 0);
    let stored = f
        .engine
        .peek("sales_invoice", doc.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.state(), Some(spot_app_api::DocState::Draft));
    let receipts = f
        .engine
        .document_receipts(&inv(), "sales_invoice", &name_of(&doc))
        .await
        .unwrap();
    assert_eq!(receipts.len(), 1, "only the create");
    assert!(receipts[0].receipt.effects.is_empty());
}

#[tokio::test]
async fn an_undeclared_effect_fails_the_operation_in_strict_mode() {
    let f = fixture_with(true, None, None).await;
    let doc = draft(&f).await;
    let before = outbox(&f.engine).await.len();
    f.knobs.rogue.store(true, Ordering::SeqCst);
    let error = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Internal);
    assert_eq!(error.details["class"], "secret_side_channel");
    assert_eq!(outbox(&f.engine).await.len(), before, "rolled back");
    // The event is logged by name only.
    let log = f.engine.undeclared_effects();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].class, class("secret_side_channel"));
    assert_eq!(log[0].operation, Operation::Post);
    assert_eq!(log[0].step, step("book_post"));
}

#[tokio::test]
async fn an_undeclared_effect_is_kept_and_flagged_when_strict_effects_is_off() {
    let f = fixture_with(false, None, None).await;
    let doc = draft(&f).await;
    f.knobs.rogue.store(true, Ordering::SeqCst);
    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    let record = &posted.receipt;
    let flagged: Vec<_> = (0..record.receipt.effects.len())
        .filter(|i| record.is_undeclared(*i))
        .collect();
    assert_eq!(flagged.len(), 1);
    assert_eq!(
        record.receipt.effects[flagged[0]].class,
        class("secret_side_channel")
    );
    assert_eq!(f.engine.undeclared_effects().len(), 1);
    // The flag is stored with the receipt.
    let read = f
        .engine
        .get_receipt(
            &inv(),
            "sales_invoice",
            &name_of(&doc),
            version(&posted.outcome.value),
            Operation::Post,
        )
        .await
        .unwrap();
    assert_eq!(read.undeclared, record.undeclared);
}

#[tokio::test]
async fn an_operation_that_reports_without_limit_fails_and_rolls_back() {
    let f = fixture().await;
    let doc = draft(&f).await;
    let before = outbox(&f.engine).await.len();
    f.knobs.flood.store(true, Ordering::SeqCst);
    let error = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap_err();
    assert_eq!(error.code, "platform.event");
    assert_eq!(outbox(&f.engine).await.len(), before);
}

#[tokio::test]
async fn phase_four_reports_move_the_state_forward_and_never_back() {
    // Three deliveries of the committed step: held, released, delivered.
    let f = fixture_with(true, Some(Arc::new(Repeat(3))), None).await;
    f.knobs.mail_states.lock().unwrap().extend([
        ReceiptState::Held,
        ReceiptState::Released,
        ReceiptState::Delivered,
    ]);
    let doc = draft(&f).await;
    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    // One entry, not three, in its final state.
    let mails = external(&posted.receipt);
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].state, ReceiptState::Delivered);
    // The announcements are in the outbox; the stored receipt folds them to the same.
    assert_eq!(topic_count(&f.engine, TOPIC_RECEIPT_EFFECT).await, 3);
    let read = f
        .engine
        .get_receipt(
            &inv(),
            "sales_invoice",
            &name_of(&doc),
            version(&posted.outcome.value),
            Operation::Post,
        )
        .await
        .unwrap();
    assert_eq!(read, posted.receipt);

    // A late, older report never lowers the state: delivered, then held again.
    let g = fixture_with(true, Some(Arc::new(Repeat(2))), None).await;
    g.knobs
        .mail_states
        .lock()
        .unwrap()
        .extend([ReceiptState::Delivered, ReceiptState::Held]);
    let doc = draft(&g).await;
    let posted = g
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    assert_eq!(external(&posted.receipt)[0].state, ReceiptState::Delivered);
}

#[tokio::test]
async fn a_failed_delivery_keeps_a_stable_code_and_a_retry_can_succeed() {
    let f = fixture_with(true, Some(Arc::new(Repeat(1))), None).await;
    // Free text from a peer is not a code: it is replaced, never stored.
    f.knobs
        .mail_states
        .lock()
        .unwrap()
        .push_back(ReceiptState::Failed {
            code: "550 mailbox full for ada@example.org token=abc123".to_owned(),
        });
    let doc = draft(&f).await;
    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    let state = &external(&posted.receipt)[0].state;
    assert_eq!(
        *state,
        ReceiptState::Failed {
            code: "reported_failure".to_owned()
        }
    );
    for event in outbox(&f.engine).await {
        assert!(!event.payload.contains("ada@example.org"));
        assert!(!event.payload.contains("abc123"));
    }

    // A code with the shape of one is kept.
    let g = fixture_with(true, Some(Arc::new(Repeat(2))), None).await;
    g.knobs.mail_states.lock().unwrap().extend([
        ReceiptState::Failed {
            code: "mail.refused".to_owned(),
        },
        ReceiptState::Delivered,
    ]);
    let doc = draft(&g).await;
    let posted = g
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    // Failed and delivered are the same rank: the retry's delivery wins.
    assert_eq!(external(&posted.receipt)[0].state, ReceiptState::Delivered);
}

#[tokio::test]
async fn idempotent_redelivery_does_not_duplicate_entries() {
    let f = fixture_with(true, Some(Arc::new(Repeat(4))), None).await;
    let doc = draft(&f).await;
    let posted = f
        .engine
        .post_with_receipt(&inv(), "sales_invoice", doc.id(), version(&doc))
        .await
        .unwrap();
    // Phase 3's entry once, the external one once, however often delivery repeated.
    assert_eq!(posted.receipt.receipt.effects.len(), 2);
    let read = f
        .engine
        .get_receipt(
            &inv(),
            "sales_invoice",
            &name_of(&doc),
            version(&posted.outcome.value),
            Operation::Post,
        )
        .await
        .unwrap();
    assert_eq!(read.receipt.effects.len(), 2);
    assert_eq!(read, posted.receipt);
}

#[test]
fn the_digest_covers_ids_and_versions_in_any_order() {
    let a = rows_digest(&[("row-1", Version(1)), ("row-2", Version(3))]).unwrap();
    let b = rows_digest(&[("row-2", Version(3)), ("row-1", Version(1))]).unwrap();
    assert_eq!(a, b, "the order rows are given in does not matter");
    assert_eq!(a.as_str().len(), 64);
    assert_ne!(
        a,
        rows_digest(&[("row-1", Version(2)), ("row-2", Version(3))]).unwrap(),
        "a version changes the digest"
    );
    assert_ne!(
        a,
        rows_digest(&[("row-1", Version(1))]).unwrap(),
        "a missing row changes the digest"
    );
    assert_ne!(
        rows_digest(&[("a", Version(1)), ("bc", Version(1))]).unwrap(),
        rows_digest(&[("ab", Version(1)), ("c", Version(1))]).unwrap(),
        "ids are length-prefixed, so they cannot run together"
    );
    // The empty set has a digest too.
    assert_eq!(rows_digest(&[]).unwrap().as_str().len(), 64);
}
