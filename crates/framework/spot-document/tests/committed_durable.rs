//! Durable delivery of the committed steps (WP-DOC-04.T4): the job is written in the operation's
//! own transaction, on the fake backend. The job handler's tests (step order, resume, ordering)
//! are in `spot-committed-delivery`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
mod flaky;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{meta, n, now};
use flaky::{Fault, FlakyBackend};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId, SiteId};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, Document, DocumentPatch, ErrorClass, HookPoint,
    Phase, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{
    CommittedBatch, CommittedDelivery, CommittedJobArgs, CommittedSteps, DurableDelivery, Engine,
    InProcessDelivery, JOB_COMMITTED_STEPS, OutboxSink, RetryPolicy, TxParticipant, V7Ids,
    WriteRecord, job_from_event,
};
use spot_events_contract::lifecycle::Operation;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, ErrorKind, FeedPosition, OutboxEvent, Transaction};
use spot_storage_fake::FakeBackend;

type Flaky = FlakyBackend<FakeBackend>;

/// The secret-looking value of a field; it must never appear in a job.
const SECRET: &str = "Zebra-Quartz-4471";

/// A controller of `customer` with committed steps at create and update, counting runs.
struct Probe {
    runs: Arc<AtomicU32>,
}

fn at(phase: Phase, op: Operation) -> HookPoint {
    HookPoint::of(phase, Target::Op(op))
}

fn decl(name: &str, point: HookPoint) -> StepDecl {
    StepDecl {
        name: StepName::new(name).unwrap(),
        point,
        priority: 0,
        budget: Duration::from_secs(5),
    }
}

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![
            decl("probe:created", at(Phase::Committed, Operation::Create)),
            decl("probe:updated", at(Phase::Committed, Operation::Update)),
            decl("probe:deleted", at(Phase::Committed, Operation::Delete)),
            decl("probe:children", at(Phase::After, Operation::Create)),
        ]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        if step.as_str() == "probe:children" {
            return Box::pin(async move {
                if ctx.doc().text("name")? == Some("Forger") {
                    ctx.enqueue_job(spot_app_api::context::JobRequest {
                        job: JOB_COMMITTED_STEPS.to_owned(),
                        args: serde_json::json!({}),
                        idempotency_key: None,
                        not_before: None,
                    })?;
                }
                if ctx.doc().text("name")? == Some("Parent") {
                    let mut child = Document::new(n("customer"), RecordId::new_v7());
                    child.set(n("name"), "Child");
                    ctx.create(child).await?;
                }
                Ok(())
            });
        }
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// Fails the write of a customer called `name`, after the storage write.
struct FailAfterWrite {
    name: &'static str,
}

impl<Tx: Transaction> TxParticipant<Tx> for FailAfterWrite {
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if write.new.and_then(|d| d.text("name").ok().flatten()) == Some(self.name) {
                return Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    "test.refused",
                    "refused",
                ));
            }
            Ok(())
        })
    }
}

/// A delivery that enqueues the job and then fails, as a participant later in the transaction
/// would: the job it wrote must go with the rollback.
struct EnqueueThenFail(DurableDelivery);

impl CommittedDelivery for EnqueueThenFail {
    fn prepare<'a>(
        &'a self,
        outbox: &'a mut dyn OutboxSink,
        batch: &'a CommittedBatch,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.0.prepare(outbox, batch).await?;
            Err(AppError::new(
                ErrorClass::Internal,
                "test.after_enqueue",
                "failed after the job was written",
            ))
        })
    }

    fn deliver<'a>(
        &'a self,
        steps: &'a dyn CommittedSteps,
        batch: CommittedBatch,
    ) -> BoxFuture<'a, Vec<spot_app_api::Message>> {
        self.0.deliver(steps, batch)
    }
}

struct Fx {
    engine: Engine<Flaky>,
    runs: Arc<AtomicU32>,
}

fn no_wait() -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    }
}

async fn fx_with(delivery: Option<Arc<dyn CommittedDelivery>>) -> Fx {
    let runs = Arc::new(AtomicU32::new(0));
    let probe = Arc::new(Probe { runs: runs.clone() });
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", probe);
    let engine = Engine::new(
        FlakyBackend::new(FakeBackend::new()),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(no_wait())
    .with_delivery(delivery.unwrap_or_else(|| Arc::new(DurableDelivery::new(Arc::new(V7Ids)))));
    engine.prepare().await.unwrap();
    Fx { engine, runs }
}

async fn fx() -> Fx {
    fx_with(None).await
}

fn inv_for(site: &str) -> Invocation {
    Invocation {
        site: SiteId::new(site).unwrap(),
        actor: Actor::System {
            component: n("durable_tests"),
        },
        command_id: CommandId::new("cmd-7").ok(),
        language: Some("en".to_owned()),
        now: now(),
    }
}

fn inv() -> Invocation {
    inv_for("durable.test")
}

fn customer(name: &str) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

impl Fx {
    async fn outbox(&self) -> Vec<OutboxEvent> {
        self.engine
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

    /// The committed-steps jobs in the outbox, decoded the way the host's intake does.
    async fn jobs(&self) -> Vec<(CommittedJobArgs, Option<String>)> {
        self.outbox()
            .await
            .iter()
            .filter_map(|e| job_from_event(e).unwrap())
            .map(|j| {
                assert_eq!(j.job, JOB_COMMITTED_STEPS);
                (
                    CommittedJobArgs::from_json(&j.args).unwrap(),
                    j.idempotency_key,
                )
            })
            .collect()
    }

    async fn topics(&self) -> Vec<String> {
        self.outbox()
            .await
            .iter()
            .map(|e| e.topic.as_str().to_owned())
            .collect()
    }
}

fn rename(doc: &Document, name: &str) -> DocumentPatch {
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("name"), name.into());
    let _ = doc;
    patch
}

#[tokio::test]
async fn a_committed_create_enqueues_exactly_one_job_and_runs_no_step() {
    let f = fx().await;
    let doc = customer("Acme");
    let id = doc.id();
    let saved = f.engine.create(&inv(), doc).await.unwrap().value;
    assert_eq!(saved.version().unwrap().0, 1);
    assert_eq!(f.topics().await, ["doc_created", "sys_job_enqueued"]);
    assert_eq!(f.runs.load(Ordering::SeqCst), 0, "delivery runs no step");
    let jobs = f.jobs().await;
    assert_eq!(jobs.len(), 1);
    let (args, key) = &jobs[0];
    assert_eq!(args.doctype, "customer");
    assert_eq!(args.id, id);
    assert_eq!(args.version, 1);
    assert_eq!(args.operation, Operation::Create);
    assert_eq!(args.site.as_str(), "durable.test");
    assert_eq!(args.now, now());
    assert_eq!(args.command_id, CommandId::new("cmd-7").ok());
    // Only the points a step is planned at.
    assert_eq!(args.steps, ["create_committed"]);
    assert_eq!(
        key.as_deref(),
        Some(format!("cmd-7:create:{id}:1").as_str())
    );
}

#[tokio::test]
async fn the_job_payload_carries_no_field_value() {
    let f = fx().await;
    let created = f
        .engine
        .create(&inv(), customer(SECRET))
        .await
        .unwrap()
        .value;
    f.engine
        .update(
            &inv(),
            "customer",
            created.id(),
            rename(&created, "Zebra-Quartz-9999"),
            created.version().unwrap(),
        )
        .await
        .unwrap();
    f.engine
        .delete(
            &inv(),
            "customer",
            created.id(),
            created.version().unwrap().next(),
        )
        .await
        .unwrap();
    let mut seen = 0;
    for event in f.outbox().await {
        if event.topic.as_str() != "sys_job_enqueued" {
            continue;
        }
        seen += 1;
        assert!(!event.payload.contains(SECRET), "{}", event.payload);
        assert!(!event.payload.contains("Zebra-Quartz"), "{}", event.payload);
    }
    assert_eq!(seen, 3, "one job per write that has committed steps");
}

#[tokio::test]
async fn a_rolled_back_operation_enqueues_no_job() {
    let f = fx().await;
    // Refused by validation: nothing is written, nothing is enqueued.
    let refused = f.engine.create(&inv(), customer(&"x".repeat(40))).await;
    assert!(refused.is_err());
    assert!(f.outbox().await.is_empty());
    // A commit that fails and is retried leaves the failed attempt's job behind in no case.
    f.engine
        .backend()
        .inject(Fault::rolled_back(ErrorKind::SerializationFailure));
    f.engine.create(&inv(), customer("Acme")).await.unwrap();
    assert_eq!(f.engine.backend().begins(), 3);
    assert_eq!(f.jobs().await.len(), 1, "the retry enqueued the one job");
}

#[tokio::test]
async fn a_failing_later_participant_rolls_the_job_back_with_the_document() {
    let f = fx().await;
    f.engine
        .set_participants(vec![Arc::new(FailAfterWrite { name: "Child" })]);
    // The parent's after_create step creates the child, whose participant fails: the parent, the
    // child, their events and their jobs are all gone.
    let parent = customer("Parent");
    let id = parent.id();
    let err = f.engine.create(&inv(), parent).await.unwrap_err();
    assert_eq!(err.code, "test.refused");
    assert!(f.engine.peek("customer", id).await.unwrap().is_none());
    assert!(f.outbox().await.is_empty(), "{:?}", f.topics().await);
}

#[tokio::test]
async fn a_failure_after_the_job_was_written_removes_the_job() {
    let delivery = Arc::new(EnqueueThenFail(DurableDelivery::new(Arc::new(V7Ids))));
    let f = fx_with(Some(delivery)).await;
    let doc = customer("Acme");
    let id = doc.id();
    let err = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_eq!(err.code, "test.after_enqueue");
    assert!(f.engine.peek("customer", id).await.unwrap().is_none());
    assert!(
        f.outbox().await.is_empty(),
        "the job went with the rollback"
    );
}

#[tokio::test]
async fn nested_writes_get_their_own_jobs_in_the_order_they_were_made() {
    let f = fx().await;
    let parent = customer("Parent");
    let parent_id = parent.id();
    f.engine.create(&inv(), parent).await.unwrap();
    let jobs = f.jobs().await;
    // The parent's step created a child in the same transaction: the child's write was made
    // first (it is inside the parent's after_create), and each write has its own job.
    assert_eq!(jobs.len(), 2);
    assert_ne!(jobs[0].0.id, jobs[1].0.id);
    assert!(jobs.iter().any(|(a, _)| a.id == parent_id));
    let keys: Vec<_> = jobs.iter().filter_map(|(_, k)| k.clone()).collect();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
}

#[tokio::test]
async fn updates_get_one_job_each_with_rising_versions() {
    let f = fx().await;
    let created = f
        .engine
        .create(&inv(), customer("Acme"))
        .await
        .unwrap()
        .value;
    let v2 = f
        .engine
        .update(
            &inv(),
            "customer",
            created.id(),
            rename(&created, "Acme 2"),
            created.version().unwrap(),
        )
        .await
        .unwrap()
        .value;
    f.engine
        .update(
            &inv(),
            "customer",
            created.id(),
            rename(&created, "Acme 3"),
            v2.version().unwrap(),
        )
        .await
        .unwrap();
    let jobs = f.jobs().await;
    let versions: Vec<_> = jobs.iter().map(|(a, _)| (a.operation, a.version)).collect();
    assert_eq!(
        versions,
        [
            (Operation::Create, 1),
            (Operation::Update, 2),
            (Operation::Update, 3)
        ]
    );
    // `update` has update_committed planned; save_committed has no step and is left out.
    assert_eq!(jobs[1].0.steps, ["update_committed"]);
}

#[tokio::test]
async fn a_deletion_names_the_last_version_before_it() {
    let f = fx().await;
    let created = f
        .engine
        .create(&inv(), customer("Acme"))
        .await
        .unwrap()
        .value;
    f.engine
        .delete(&inv(), "customer", created.id(), created.version().unwrap())
        .await
        .unwrap();
    let jobs = f.jobs().await;
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[1].0.operation, Operation::Delete);
    assert_eq!(jobs[1].0.version, 1);
    assert!(
        jobs[0].0.order() < jobs[1].0.order(),
        "the deletion sorts after the create"
    );
}

#[tokio::test]
async fn the_same_operation_produces_the_same_job_and_each_site_its_own() {
    let a = fx().await;
    let b = fx().await;
    let doc = customer("Acme");
    a.engine.create(&inv(), doc.clone()).await.unwrap();
    b.engine.create(&inv(), doc.clone()).await.unwrap();
    assert_eq!(a.jobs().await, b.jobs().await, "deterministic");
    let c = fx().await;
    c.engine.create(&inv_for("other.test"), doc).await.unwrap();
    let jobs = c.jobs().await;
    assert_eq!(jobs[0].0.site.as_str(), "other.test");
    assert_ne!(jobs[0].0, a.jobs().await[0].0);
}

#[tokio::test]
async fn the_in_process_delivery_still_runs_the_steps_and_enqueues_nothing() {
    let f = fx_with(Some(Arc::new(InProcessDelivery))).await;
    f.engine.create(&inv(), customer("Acme")).await.unwrap();
    assert_eq!(f.runs.load(Ordering::SeqCst), 1);
    assert_eq!(f.topics().await, ["doc_created"]);
}

#[test]
fn the_arguments_round_trip_and_unknown_shapes_are_refused() {
    let args = CommittedJobArgs {
        site: SiteId::new("durable.test").unwrap(),
        doctype: "customer".to_owned(),
        id: RecordId::new_v7(),
        version: 4,
        operation: Operation::Post,
        command_id: None,
        actor: Actor::System {
            component: n("durable_tests"),
        },
        now: now(),
        language: None,
        steps: vec!["post_committed".to_owned()],
    };
    let json = args.to_json().unwrap();
    assert_eq!(CommittedJobArgs::from_json(&json).unwrap(), args);
    assert_eq!(args.idempotency_key(), format!("-:post:{}:4", args.id));
    let mut other = json.clone();
    other["schema"] = 2.into();
    assert!(CommittedJobArgs::from_json(&other).is_err());
    let mut other = json;
    other["operation"] = "explode".into();
    assert!(CommittedJobArgs::from_json(&other).is_err());
    let _ = Mutex::new(());
}

#[tokio::test]
async fn an_app_hook_cannot_enqueue_a_platform_job() {
    // A forged `sys:committed_steps` job would run steps as any actor it names.
    let f = fx().await;
    let err = f
        .engine
        .create(&inv(), customer("Forger"))
        .await
        .expect_err("the hook's enqueue is refused");
    assert_eq!(err.code, "platform.invalid_job_name");
    assert!(f.jobs().await.is_empty());
}
