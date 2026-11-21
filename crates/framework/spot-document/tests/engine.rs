//! The engine's own behaviour: retries, the outbox, rollback, savepoints and delivery.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
mod flaky;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{meta, n, now};
use flaky::{Fault, FlakyBackend};
use serde_json::json;
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation, JobRequest};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, DocumentPatch, ErrorClass, HookPoint, Phase, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{
    CommittedBatch, CommittedDelivery, CommittedSteps, Engine, RetryPolicy, V7Ids, job_from_event,
};
use spot_events_contract::PayloadDocument;
use spot_events_contract::lifecycle::Operation;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, ErrorKind, FeedPosition, OutboxEvent};
use spot_storage_fake::FakeBackend;

type Flaky = FlakyBackend<FakeBackend>;
type StepFn =
    Arc<dyn for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> + Send + Sync>;

/// A controller of `customer` built from closures.
struct Probe {
    steps: Vec<(StepDecl, StepFn)>,
}

impl Probe {
    fn new() -> Self {
        Self { steps: Vec::new() }
    }

    fn step<F>(mut self, name: &str, point: HookPoint, f: F) -> Self
    where
        F: for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>>
            + Send
            + Sync
            + 'static,
    {
        let decl = StepDecl {
            name: StepName::new(name).unwrap(),
            point,
            priority: 0,
            budget: Duration::from_secs(5),
        };
        self.steps.push((decl, Arc::new(f)));
        self
    }
}

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps.iter().map(|(d, _)| d.clone()).collect()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        match self.steps.iter().find(|(d, _)| &d.name == step) {
            Some((_, f)) => f(ctx),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

fn at(phase: Phase, op: Operation) -> HookPoint {
    HookPoint::of(phase, Target::Op(op))
}

fn no_wait() -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    }
}

async fn engine_with(probe: Probe) -> Engine<Flaky> {
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", Arc::new(probe));
    let engine = Engine::new(
        FlakyBackend::new(FakeBackend::new()),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(no_wait());
    engine.prepare().await.unwrap();
    engine
}

async fn engine() -> Engine<Flaky> {
    engine_with(Probe::new()).await
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("engine.test").unwrap(),
        actor: Actor::System {
            component: n("engine_tests"),
        },
        command_id: CommandId::new("cmd-7").ok(),
        language: None,
        now: now(),
    }
}

fn customer(name: &str) -> spot_app_api::Document {
    let mut doc = spot_app_api::Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

fn version(doc: &spot_app_api::Document) -> Version {
    doc.version().unwrap()
}

async fn outbox(engine: &Engine<Flaky>) -> Vec<OutboxEvent> {
    engine
        .backend()
        .read_outbox(FeedPosition::START, 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.event)
        // The journal's state images (WP-JRN-02.T2) are not what these tests count.
        .filter(|e| e.topic.as_str() != "doc_state")
        .collect()
}

fn topics(events: &[OutboxEvent]) -> Vec<&str> {
    events.iter().map(|e| e.topic.as_str()).collect()
}

fn job(name: &str) -> JobRequest {
    JobRequest {
        job: name.to_owned(),
        args: json!({"n": 1}),
        idempotency_key: Some("k".into()),
        not_before: None,
    }
}

#[tokio::test]
async fn a_serialization_failure_is_retried_as_a_whole_and_the_retry_is_recorded() {
    let runs = Arc::new(AtomicU32::new(0));
    let counted = runs.clone();
    let engine = engine_with(Probe::new().step(
        "probe:count",
        at(Phase::Before, Operation::Create),
        move |_ctx| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        },
    ))
    .await;
    engine
        .backend()
        .inject(Fault::rolled_back(ErrorKind::SerializationFailure));
    let doc = customer("Acme");
    let id = doc.id();
    let saved = engine.create(&inv(), doc).await.unwrap().value;
    assert_eq!(version(&saved), Version::FIRST);
    assert_eq!(
        engine.backend().begins(),
        2,
        "one failed attempt and one retry"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the whole operation ran again"
    );
    let retries = engine.retries();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].label, "customer_create");
    assert_eq!(retries[0].attempt, 1);
    assert_eq!(retries[0].cause, ErrorKind::SerializationFailure);
    assert!(engine.peek("customer", id).await.unwrap().is_some());
    assert_eq!(
        topics(&outbox(&engine).await),
        ["doc_created"],
        "the failed attempt left no event"
    );
}

#[tokio::test]
async fn retries_stop_after_three_and_the_error_says_to_try_again() {
    let engine = engine().await;
    for _ in 0..4 {
        engine
            .backend()
            .inject(Fault::rolled_back(ErrorKind::Deadlock));
    }
    let doc = customer("Acme");
    let id = doc.id();
    let e = engine.create(&inv(), doc).await.unwrap_err();
    assert_eq!(e.code, "server.conflict_retry_exhausted");
    assert!(e.retryable());
    assert_eq!(
        engine.backend().begins(),
        4,
        "the first attempt and three retries"
    );
    assert_eq!(engine.retries().len(), 3);
    assert!(engine.peek("customer", id).await.unwrap().is_none());
    assert!(outbox(&engine).await.is_empty());
}

#[tokio::test]
async fn an_unknown_commit_outcome_is_never_retried() {
    let engine = engine().await;
    engine.backend().inject(Fault {
        kind: ErrorKind::CommitOutcomeUnknown,
        commit_first: true,
    });
    let e = engine.create(&inv(), customer("Acme")).await.unwrap_err();
    assert_eq!(e.api_code(), "server.unavailable");
    assert!(e.retryable(), "the client retries with its idempotency key");
    assert_eq!(engine.backend().begins(), 1, "the engine did not retry");
    assert!(engine.retries().is_empty());
}

#[tokio::test]
async fn the_backoff_doubles_up_to_its_limit() {
    let policy = RetryPolicy::default();
    assert_eq!(policy.delay(1), Duration::from_millis(10));
    assert_eq!(policy.delay(2), Duration::from_millis(20));
    assert_eq!(policy.delay(3), Duration::from_millis(40));
    assert_eq!(policy.delay(30), Duration::from_millis(200));
}

#[tokio::test]
async fn one_lifecycle_event_per_successful_operation_and_none_for_a_failed_one() {
    let engine = engine().await;
    let created = engine.create(&inv(), customer("Acme")).await.unwrap().value;
    let id = created.id();
    let patch = DocumentPatch {
        fields: [(n("name"), Value::Text("Acme Ltd".into()))].into(),
        ..Default::default()
    };
    let updated = engine
        .update(&inv(), "customer", id, patch.clone(), version(&created))
        .await
        .unwrap()
        .value;
    // Each of these fails: a stale version, an invalid value, a missing document.
    engine
        .update(&inv(), "customer", id, patch, version(&created))
        .await
        .unwrap_err();
    let too_long = DocumentPatch {
        fields: [(n("name"), Value::Text("x".repeat(40)))].into(),
        ..Default::default()
    };
    let e = engine
        .update(&inv(), "customer", id, too_long, version(&updated))
        .await
        .unwrap_err();
    assert_eq!(e.api_code(), "document.validation_failed");
    engine
        .delete(&inv(), "customer", RecordId::new_v7(), Version::FIRST)
        .await
        .unwrap_err();
    engine
        .delete(&inv(), "customer", id, version(&updated))
        .await
        .unwrap();

    let events = outbox(&engine).await;
    assert_eq!(
        topics(&events),
        ["doc_created", "doc_updated", "doc_deleted"]
    );
    let update = PayloadDocument::from_outbox_event(&events[1]).unwrap();
    assert_eq!(update.data, json!({"changed_fields": ["name"]}));
    assert_eq!(
        update.meta.command_id.as_ref().map(|c| c.as_str()),
        Some("cmd-7")
    );
    assert_eq!(update.meta.occurred_at, now());
    assert_eq!(events[0].subject_version, Some(Version(1)));
    assert_eq!(events[1].subject_version, Some(Version(2)));
    assert_eq!(
        events[2].subject_version,
        Some(Version(2)),
        "a delete reports the last version before deletion"
    );
    assert!(
        events
            .iter()
            .all(|e| e.schema_version == 1 && e.subject.id == id)
    );
}

#[tokio::test]
async fn a_lost_update_is_refused_with_the_current_version() {
    let engine = engine().await;
    let created = engine.create(&inv(), customer("Acme")).await.unwrap().value;
    let patch = |name: &str| DocumentPatch {
        fields: [(n("name"), Value::Text(name.into()))].into(),
        ..Default::default()
    };
    engine
        .update(
            &inv(),
            "customer",
            created.id(),
            patch("One"),
            version(&created),
        )
        .await
        .unwrap();
    let e = engine
        .update(
            &inv(),
            "customer",
            created.id(),
            patch("Two"),
            version(&created),
        )
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::StaleVersion);
    assert_eq!(e.details.get("current_version"), Some(&json!(2)));
    let kept = engine
        .peek("customer", created.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.get("name"), &Value::Text("One".into()));
}

#[tokio::test]
async fn jobs_enqueued_in_phase_3_reach_the_outbox_and_vanish_on_rollback() {
    let engine = engine_with(
        Probe::new()
            .step(
                "probe:enqueue",
                at(Phase::After, Operation::Create),
                |ctx| {
                    Box::pin(async move {
                        ctx.enqueue_job(job("probe:follow_up"))?;
                        ctx.notify_user(RecordId::new_v7(), json!({"hello": true}))
                    })
                },
            )
            .step("probe:fail", at(Phase::After, Operation::Create), |ctx| {
                Box::pin(async move {
                    if ctx.doc().text("name")? == Some("Fail") {
                        return Err(AppError::new(ErrorClass::Validation, "probe.failed", "no"));
                    }
                    Ok(())
                })
            }),
    )
    .await;
    engine.create(&inv(), customer("Fine")).await.unwrap();
    let events = outbox(&engine).await;
    assert_eq!(
        topics(&events),
        ["doc_created", "sys_job_enqueued", "sys_user_notice"],
        "the lifecycle event first, then what the step enqueued, in order"
    );
    let request = job_from_event(&events[1]).unwrap().unwrap();
    assert_eq!(request.job, "probe:follow_up");
    assert_eq!(request.args, json!({"n": 1}));
    assert_eq!(request.idempotency_key.as_deref(), Some("k"));

    engine.create(&inv(), customer("Fail")).await.unwrap_err();
    assert_eq!(
        outbox(&engine).await.len(),
        3,
        "the rolled-back operation left no job, notice or lifecycle event"
    );
}

#[tokio::test]
async fn a_nested_failure_the_step_handles_leaves_nothing_behind() {
    // `after_create` of a customer creates a second customer that fails validation, handles the
    // error, and then creates a valid one. The failed write ran in a savepoint.
    let made = Arc::new(Mutex::new(Vec::new()));
    let seen = made.clone();
    let engine = engine_with(Probe::new().step(
        "probe:children",
        at(Phase::After, Operation::Create),
        move |ctx| {
            let seen = seen.clone();
            Box::pin(async move {
                if ctx.doc().text("name")? != Some("Parent") {
                    return Ok(());
                }
                ctx.enqueue_job(job("probe:from_parent"))?;
                let mut bad = spot_app_api::Document::new(n("customer"), RecordId::new_v7());
                bad.set(n("name"), "x".repeat(40));
                let refused = ctx.create(bad).await.unwrap_err();
                assert_eq!(refused.api_code(), "document.validation_failed");
                let mut good = spot_app_api::Document::new(n("customer"), RecordId::new_v7());
                good.set(n("name"), "Child");
                let saved = ctx.create(good).await?;
                seen.lock().unwrap().push(saved.id());
                Ok(())
            })
        },
    ))
    .await;
    engine.create(&inv(), customer("Parent")).await.unwrap();
    let ids = made.lock().unwrap().clone();
    assert_eq!(ids.len(), 1);
    assert!(engine.peek("customer", ids[0]).await.unwrap().is_some());
    assert_eq!(
        topics(&outbox(&engine).await),
        ["doc_created", "doc_created", "sys_job_enqueued"],
        "the parent, the child that was saved, then the parent's job; the refused write left no event"
    );
}

#[tokio::test]
async fn a_committed_step_enqueues_in_a_transaction_of_its_own() {
    let engine = engine_with(Probe::new().step(
        "probe:after_commit",
        at(Phase::Committed, Operation::Create),
        |ctx| Box::pin(async move { ctx.enqueue_job(job("probe:mail")) }),
    ))
    .await;
    engine.create(&inv(), customer("Acme")).await.unwrap();
    assert_eq!(
        topics(&outbox(&engine).await),
        ["doc_created", "sys_job_enqueued"]
    );
}

/// Delivery that only collects: the engine runs no committed step itself.
struct Collect(Mutex<Vec<CommittedBatch>>);

impl CommittedDelivery for Collect {
    fn deliver<'a>(
        &'a self,
        _steps: &'a dyn CommittedSteps,
        batch: CommittedBatch,
    ) -> BoxFuture<'a, Vec<spot_app_api::Message>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(batch);
            Vec::new()
        })
    }
}

#[tokio::test]
async fn committed_steps_go_through_the_delivery_the_engine_was_given() {
    let ran = Arc::new(AtomicU32::new(0));
    let counted = ran.clone();
    let delivery = Arc::new(Collect(Mutex::new(Vec::new())));
    let engine = engine_with(Probe::new().step(
        "probe:committed",
        at(Phase::Committed, Operation::Create),
        move |_ctx| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        },
    ))
    .await
    .with_delivery(delivery.clone());
    engine.create(&inv(), customer("Acme")).await.unwrap();
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the delivery decides when steps run"
    );
    let batches = delivery.0.lock().unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].invocation, inv());
    let points: Vec<String> = batches[0].steps.iter().map(|s| s.point.name()).collect();
    assert_eq!(points, ["create_committed", "save_committed"]);
}

#[tokio::test]
async fn a_committed_failure_is_recorded_with_what_it_was_about() {
    let engine = engine_with(Probe::new().step(
        "probe:boom",
        at(Phase::Committed, Operation::Create),
        |_ctx| {
            Box::pin(async {
                Err(AppError::new(
                    ErrorClass::Unavailable,
                    "probe.smtp_down",
                    "down",
                ))
            })
        },
    ))
    .await;
    let doc = customer("Acme");
    let id = doc.id();
    engine.create(&inv(), doc).await.unwrap();
    let failures = engine.committed_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].point, "create_committed");
    assert_eq!(failures[0].doctype, "customer");
    assert_eq!(failures[0].id, id);
    assert_eq!(failures[0].error.code, "probe.smtp_down");
    assert_eq!(failures[0].at, now());
}

#[tokio::test]
async fn refusals_that_need_no_step() {
    let engine = engine().await;
    let created = engine.create(&inv(), customer("Acme")).await.unwrap().value;
    // The same id again.
    let mut again = spot_app_api::Document::new(n("customer"), created.id());
    again.set(n("name"), "Twin");
    let e = engine.create(&inv(), again).await.unwrap_err();
    assert_eq!(e.api_code(), "document.duplicate");
    // A document that was saved already is not created again.
    let e = engine.create(&inv(), created.clone()).await.unwrap_err();
    assert_eq!(e.api_code(), "document.invalid_state");
    // A child DocType is not created on its own, an unknown one does not exist.
    let row = spot_app_api::Document::new(n("sales_invoice_item"), RecordId::new_v7());
    assert_eq!(
        engine.create(&inv(), row).await.unwrap_err().code,
        "document.invalid_state"
    );
    let ghost = spot_app_api::Document::new(n("ghost"), RecordId::new_v7());
    assert_eq!(
        engine.create(&inv(), ghost).await.unwrap_err().api_code(),
        "meta.unknown_doctype"
    );
    // Not found.
    let e = engine
        .update(
            &inv(),
            "customer",
            RecordId::new_v7(),
            DocumentPatch::default(),
            Version::FIRST,
        )
        .await
        .unwrap_err();
    assert_eq!(e.api_code(), "document.not_found");
    // Posting a DocType that is not posted is refused by state.
    let e = engine
        .post(&inv(), "customer", created.id(), Version::FIRST)
        .await
        .unwrap_err();
    assert_eq!(e.code, "document.invalid_state");
}

#[tokio::test]
async fn a_hook_posts_another_document_through_its_own_lifecycle() {
    // `after_create` of the customer creates a posted invoice through the repository: the
    // nested write runs the invoice's full create lifecycle and then its post lifecycle, both
    // in the caller's transaction (WP-DOC-04.T2).
    let engine = engine_with(Probe::new().step(
        "probe:post_invoice",
        at(Phase::After, Operation::Create),
        |ctx| {
            Box::pin(async move {
                if ctx.doc().text("name")? != Some("Poster") {
                    return Ok(());
                }
                let mut invoice = common::valid_invoice();
                invoice.set(n("customer"), Value::Id(ctx.doc().id()));
                let saved = ctx.create(invoice).await?;
                assert_eq!(saved.state(), Some(spot_app_api::DocState::Draft));
                let posted = ctx
                    .post("sales_invoice", saved.id(), version(&saved))
                    .await?;
                assert_eq!(posted.state(), Some(spot_app_api::DocState::Posted));
                Ok(())
            })
        },
    ))
    .await;
    engine.create(&inv(), customer("Poster")).await.unwrap();
    assert_eq!(
        topics(&outbox(&engine).await),
        ["doc_created", "doc_created", "doc_posted"],
        "the customer, the invoice it created, then the invoice's post"
    );
}

#[tokio::test]
async fn the_engine_follows_new_metadata_and_registry_for_the_next_operation() {
    let engine = engine().await;
    engine.create(&inv(), customer("Acme")).await.unwrap();
    // Metadata with one more DocType.
    let mut defs = common::definitions();
    let mut note = spot_meta::Definition::new("note");
    note.fields = vec![spot_meta::FieldSpec::new(
        "text",
        spot_meta::FieldType::Text,
    )];
    defs.push(note);
    let meta = spot_meta::Registry::new(defs).unwrap().effective();
    engine.set_meta(meta).await.unwrap();
    let mut doc = spot_app_api::Document::new(n("note"), RecordId::new_v7());
    doc.set(n("text"), "hello");
    engine.create(&inv(), doc).await.unwrap();
    // A registry with a step that refuses every create.
    let mut builder = RegistryBuilder::new();
    builder.add_controller(
        "probe",
        Arc::new(Probe::new().step(
            "probe:refuse",
            at(Phase::Validate, Operation::Create),
            |_ctx| {
                Box::pin(async {
                    Err(AppError::new(
                        ErrorClass::Validation,
                        "probe.closed",
                        "closed",
                    ))
                })
            },
        )),
    );
    engine.set_seams(Arc::new(builder.build().unwrap()));
    let e = engine.create(&inv(), customer("Late")).await.unwrap_err();
    assert_eq!(e.code, "probe.closed");
}
