//! The committed-steps job from the job engine's side (WP-DOC-04.T4): the engine writes the job
//! in its transaction, the outbox feeds an `InMemoryJobStore` the way the host's intake does,
//! and `CommittedStepsHandler` runs the steps. Fake storage backend throughout.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, Document, DocumentPatch, ErrorClass, HookPoint,
    Phase, Target,
};
use spot_committed_delivery::{
    CommittedStepsHandler, InMemoryMarkers, JobStoreMarkers, MarkerError, SingleSite, StepMarkers,
};
use spot_core::ManualClock;
use spot_document::engine::{
    DurableDelivery, Engine, JOB_COMMITTED_STEPS, RetryPolicy, V7Ids, job_from_event,
};
use spot_events_contract::lifecycle::Operation;
use spot_jobs::model::Claim;
use spot_jobs::store::JobFilter;
use spot_jobs::worker::{HandlerResult, JobContext};
use spot_jobs::{InMemoryJobStore, Job, JobId, JobState, JobStore, NewJob};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, FeedPosition};
use spot_storage_fake::FakeBackend;

const SITE: &str = "durable.test";

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap()
}

/// How a step answers.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Behaviour {
    Ok,
    /// A business refusal, always.
    Refuse,
    /// A temporary infrastructure error on the first call only.
    UnavailableOnce,
}

#[derive(Default)]
struct Script {
    /// `(point, name seen, command id seen)` per call, in order.
    calls: Vec<(String, Option<String>, Option<String>)>,
    behaviour: BTreeMap<String, Behaviour>,
    failed_once: Vec<String>,
}

type Shared = Arc<Mutex<Script>>;

struct Probe {
    script: Shared,
}

fn committed(op: Operation) -> HookPoint {
    HookPoint::of(Phase::Committed, Target::Op(op))
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
            decl("probe:created", committed(Operation::Create)),
            decl("probe:saved", HookPoint::of(Phase::Committed, Target::Save)),
            decl("probe:deleted", committed(Operation::Delete)),
        ]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        let point = ctx.point().name();
        let name = ctx.doc().text("name").ok().flatten().map(str::to_owned);
        let command = ctx.invocation().command_id.as_ref().map(|c| c.to_string());
        let outcome = {
            let mut s = self.script.lock().unwrap();
            s.calls.push((point.clone(), name, command));
            match s.behaviour.get(&point).copied().unwrap_or(Behaviour::Ok) {
                Behaviour::Ok => Ok(()),
                Behaviour::Refuse => Err(AppError::new(
                    ErrorClass::InvalidState,
                    "probe.refused",
                    "refused",
                )),
                Behaviour::UnavailableOnce if !s.failed_once.contains(&point) => {
                    s.failed_once.push(point);
                    Err(AppError::new(ErrorClass::Unavailable, "probe.down", "down"))
                }
                Behaviour::UnavailableOnce => Ok(()),
            }
        };
        Box::pin(async move { outcome })
    }
}

fn definitions() -> Vec<Definition> {
    let mut def = Definition::new("customer");
    def.title_field = Some("name".to_owned());
    let mut name = FieldSpec::new("name", FieldType::Text);
    name.required = true;
    name.max_chars = Some(20);
    def.fields.push(name);
    vec![def]
}

struct Fx {
    engine: Arc<Engine<FakeBackend>>,
    script: Shared,
    store: Arc<InMemoryJobStore>,
    markers: Arc<InMemoryMarkers>,
    handler: Arc<CommittedStepsHandler>,
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new(SITE).unwrap(),
        actor: Actor::System {
            component: n("durable_tests"),
        },
        command_id: CommandId::new("cmd-7").ok(),
        language: None,
        now: now(),
    }
}

async fn fx_with(markers: Option<Arc<dyn StepMarkers>>) -> (Fx, Arc<dyn StepMarkers>) {
    let script: Shared = Arc::default();
    let mut builder = RegistryBuilder::new();
    builder.add_controller(
        "probe",
        Arc::new(Probe {
            script: script.clone(),
        }),
    );
    let meta = Registry::new(definitions()).unwrap().effective();
    let engine = Engine::new(
        FakeBackend::new(),
        meta,
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(RetryPolicy {
        max_retries: 1,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    })
    .with_delivery(Arc::new(DurableDelivery::new(Arc::new(V7Ids))));
    engine.prepare().await.unwrap();
    let engine = Arc::new(engine);
    let store = Arc::new(InMemoryJobStore::new());
    let memory = Arc::new(InMemoryMarkers::new());
    let used: Arc<dyn StepMarkers> = markers.unwrap_or_else(|| memory.clone());
    let handler = Arc::new(CommittedStepsHandler::new(
        Arc::new(SingleSite::new(
            SiteId::new(SITE).unwrap(),
            Arc::new(engine.runner()),
        )),
        used.clone(),
        store.clone(),
    ));
    (
        Fx {
            engine,
            script,
            store,
            markers: memory,
            handler,
        },
        used,
    )
}

async fn fx() -> Fx {
    fx_with(None).await.0
}

impl Fx {
    async fn create(&self, name: &str) -> Document {
        let mut doc = Document::new(n("customer"), RecordId::new_v7());
        doc.set(n("name"), name);
        self.engine.create(&inv(), doc).await.unwrap().value
    }

    async fn rename(&self, doc: &Document, name: &str) -> Document {
        let mut patch = DocumentPatch::default();
        patch.fields.insert(n("name"), name.into());
        self.engine
            .update(&inv(), "customer", doc.id(), patch, doc.version().unwrap())
            .await
            .unwrap()
            .value
    }

    /// What the host's intake does with the outbox: each job event becomes a stored job, keyed
    /// by the event id and the idempotency key.
    async fn intake(&self) -> Vec<spot_jobs::JobId> {
        let events = self
            .engine
            .backend()
            .read_outbox(FeedPosition::START, 10_000)
            .await
            .unwrap();
        let mut ids = Vec::new();
        for record in events {
            let Some(request) = job_from_event(&record.event).unwrap() else {
                continue;
            };
            let mut job = NewJob::new(
                request.job,
                request.args,
                "default".to_owned(),
                SiteId::new(SITE).unwrap(),
                now(),
            )
            .with_outbox_event_id(record.event.id.to_string());
            if let Some(key) = request.idempotency_key {
                job = job.with_idempotency_key(key);
            }
            match self.store.insert_if_new(job).await.unwrap() {
                spot_jobs::model::InsertResult::Inserted(id)
                | spot_jobs::model::InsertResult::Duplicate(id) => ids.push(id),
            }
        }
        ids
    }

    async fn job(&self, id: JobId) -> Job {
        self.store.get(id).await.unwrap().unwrap()
    }

    /// One attempt of the job, as a worker would make it.
    async fn attempt(&self, id: JobId) -> HandlerResult {
        let job = self.job(id).await;
        self.handler
            .handle(&JobContext {
                attempt: job.attempt,
                job,
                clock: now(),
            })
            .await
    }

    /// Finishes the oldest unfinished job with `state`, as the worker's completion would.
    async fn finish_oldest(&self, state: JobState) {
        let claimed = self
            .store
            .claim(
                &["default".to_owned()],
                "w",
                Duration::from_secs(60),
                now(),
                1,
            )
            .await
            .unwrap();
        let first = claimed.into_iter().next().unwrap();
        self.store
            .complete(&first.claim, state, None)
            .await
            .unwrap();
    }

    fn calls(&self) -> Vec<String> {
        self.script
            .lock()
            .unwrap()
            .calls
            .iter()
            .map(|c| c.0.clone())
            .collect()
    }

    fn set(&self, point: &str, behaviour: Behaviour) {
        self.script
            .lock()
            .unwrap()
            .behaviour
            .insert(point.to_owned(), behaviour);
    }
}

#[tokio::test]
async fn a_committed_create_enqueues_one_job_and_the_worker_runs_its_steps_in_order() {
    let f = fx().await;
    f.create("Acme").await;
    assert!(f.calls().is_empty(), "nothing ran at the commit");
    let ids = f.intake().await;
    assert_eq!(ids.len(), 1);
    assert_eq!(f.job(ids[0]).await.name, JOB_COMMITTED_STEPS);
    assert_eq!(f.attempt(ids[0]).await, HandlerResult::Ok);
    assert_eq!(f.calls(), ["create_committed", "save_committed"]);
    // The steps saw the document, the original command id.
    let calls = f.script.lock().unwrap().calls.clone();
    assert_eq!(calls[0].1.as_deref(), Some("Acme"));
    assert_eq!(calls[0].2.as_deref(), Some("cmd-7"));
    // A repeat of the finished job runs nothing.
    assert_eq!(f.attempt(ids[0]).await, HandlerResult::Ok);
    assert_eq!(f.calls().len(), 2);
}

#[tokio::test]
async fn a_double_enqueue_is_one_job() {
    let f = fx().await;
    f.create("Acme").await;
    let first = f.intake().await;
    let second = f.intake().await;
    assert_eq!(first, second, "a replayed intake finds the same job");
    let jobs = f.store.list(JobFilter::default()).await.unwrap();
    assert_eq!(jobs.len(), 1);
    // The same step enqueued by another event is dropped by the idempotency key while pending.
    let mut again = NewJob::new(
        JOB_COMMITTED_STEPS.to_owned(),
        jobs[0].args.clone(),
        "default".to_owned(),
        SiteId::new(SITE).unwrap(),
        now(),
    )
    .with_outbox_event_id("another-event".to_owned());
    again = again.with_idempotency_key(jobs[0].idempotency_key.clone().unwrap());
    assert!(matches!(
        f.store.insert_if_new(again).await.unwrap(),
        spot_jobs::model::InsertResult::Duplicate(_)
    ));
}

#[tokio::test]
async fn a_handler_stopped_after_step_one_resumes_at_step_two() {
    let f = fx().await;
    f.set("save_committed", Behaviour::UnavailableOnce);
    f.create("Acme").await;
    let id = f.intake().await[0];
    let first = f.attempt(id).await;
    assert!(matches!(first, HandlerResult::Retry(_)), "{first:?}");
    assert_eq!(f.calls(), ["create_committed", "save_committed"]);
    assert!(
        f.markers
            .done(id)
            .await
            .unwrap()
            .contains("create_committed")
    );
    assert_eq!(f.attempt(id).await, HandlerResult::Ok);
    // Step one never ran again; step two ran again and succeeded.
    assert_eq!(
        f.calls(),
        ["create_committed", "save_committed", "save_committed"]
    );
}

#[tokio::test]
async fn a_step_refused_with_a_business_error_does_not_block_the_next_step() {
    let f = fx().await;
    f.set("create_committed", Behaviour::Refuse);
    f.create("Acme").await;
    let id = f.intake().await[0];
    assert_eq!(f.attempt(id).await, HandlerResult::Ok);
    assert_eq!(f.calls(), ["create_committed", "save_committed"]);
    let failures = f.engine.committed_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].error.code, "probe.refused");
    // The refused step is not retried forever: its marker is written.
    assert_eq!(f.attempt(id).await, HandlerResult::Ok);
    assert_eq!(f.calls().len(), 2);
}

/// Markers that fail the first write of a point: a crash between the side effect and the marker.
struct CrashBeforeMark {
    inner: Arc<dyn StepMarkers>,
    crashed: Mutex<bool>,
}

impl StepMarkers for CrashBeforeMark {
    fn done<'a>(
        &'a self,
        job: JobId,
    ) -> BoxFuture<'a, Result<std::collections::BTreeSet<String>, MarkerError>> {
        self.inner.done(job)
    }

    fn mark<'a>(
        &'a self,
        job: JobId,
        claim: Option<&'a Claim>,
        point: &'a str,
    ) -> BoxFuture<'a, Result<(), MarkerError>> {
        Box::pin(async move {
            let first = {
                let mut crashed = self.crashed.lock().unwrap();
                !std::mem::replace(&mut *crashed, true)
            };
            if first {
                return Err(MarkerError);
            }
            self.inner.mark(job, claim, point).await
        })
    }
}

#[tokio::test]
async fn a_crash_between_a_side_effect_and_its_marker_reruns_that_one_step_only() {
    let marks: Arc<dyn StepMarkers> = Arc::new(CrashBeforeMark {
        inner: Arc::new(InMemoryMarkers::new()),
        crashed: Mutex::new(false),
    });
    let (f, _) = fx_with(Some(marks)).await;
    f.create("Acme").await;
    let id = f.intake().await[0];
    assert!(matches!(f.attempt(id).await, HandlerResult::Retry(_)));
    assert_eq!(f.calls(), ["create_committed"]);
    assert_eq!(f.attempt(id).await, HandlerResult::Ok);
    // The at-least-once edge: step one ran twice, step two once.
    assert_eq!(
        f.calls(),
        ["create_committed", "create_committed", "save_committed"]
    );
}

#[tokio::test]
async fn two_updates_of_one_document_run_their_steps_in_version_order() {
    let f = fx().await;
    f.set("create_committed", Behaviour::Ok);
    let doc = f.create("v1").await;
    let v2 = f.rename(&doc, "v2").await;
    f.rename(&v2, "v3").await;
    let ids = f.intake().await;
    assert_eq!(ids.len(), 3);
    // The last write's job waits for the earlier ones, however the worker picks them.
    assert!(matches!(f.attempt(ids[2]).await, HandlerResult::Retry(_)));
    assert!(matches!(f.attempt(ids[1]).await, HandlerResult::Retry(_)));
    assert!(f.calls().is_empty());
    assert_eq!(f.attempt(ids[0]).await, HandlerResult::Ok);
    f.finish_oldest(JobState::Succeeded).await;
    assert_eq!(f.attempt(ids[1]).await, HandlerResult::Ok);
    f.finish_oldest(JobState::Succeeded).await;
    assert_eq!(f.attempt(ids[2]).await, HandlerResult::Ok);
    // Create ran create+save; each update ran its save step (no update step is declared).
    assert_eq!(
        f.calls(),
        [
            "create_committed",
            "save_committed",
            "save_committed",
            "save_committed"
        ]
    );
}

#[tokio::test]
async fn a_dead_predecessor_does_not_block_later_writes() {
    let f = fx().await;
    let doc = f.create("v1").await;
    f.rename(&doc, "v2").await;
    let ids = f.intake().await;
    f.finish_oldest(JobState::Dead).await;
    assert_eq!(f.attempt(ids[1]).await, HandlerResult::Ok);
}

#[tokio::test]
async fn a_deletion_runs_its_step_without_any_field_value() {
    let f = fx().await;
    let doc = f.create("Secret Name").await;
    f.engine
        .delete(&inv(), "customer", doc.id(), doc.version().unwrap())
        .await
        .unwrap();
    let ids = f.intake().await;
    f.attempt(ids[0]).await;
    f.finish_oldest(JobState::Succeeded).await;
    f.script.lock().unwrap().calls.clear();
    assert_eq!(f.attempt(ids[1]).await, HandlerResult::Ok);
    let calls = f.script.lock().unwrap().calls.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "delete_committed");
    assert_eq!(calls[0].1, None, "the job carries no image of the document");
}

#[tokio::test]
async fn a_job_for_another_site_runs_nothing() {
    let f = fx().await;
    f.create("Acme").await;
    let id = f.intake().await[0];
    let job = f.job(id).await;
    // The arguments name one site, the job belongs to another.
    let mut foreign = job.clone();
    foreign.site = SiteId::new("other.test").unwrap();
    let outcome = f
        .handler
        .handle(&JobContext {
            attempt: 1,
            job: foreign,
            clock: now(),
        })
        .await;
    assert_eq!(
        outcome,
        HandlerResult::Fail("committed_delivery.wrong_site".to_owned())
    );
    // A job whose arguments and site agree on a site nobody serves.
    let mut args = job.args.clone();
    args["site"] = json!("other.test");
    let mut stranger = job.clone();
    stranger.site = SiteId::new("other.test").unwrap();
    stranger.args = args;
    let outcome = f
        .handler
        .handle(&JobContext {
            attempt: 1,
            job: stranger,
            clock: now(),
        })
        .await;
    assert_eq!(
        outcome,
        HandlerResult::Fail("committed_delivery.unknown_site".to_owned())
    );
    // Arguments that do not decode.
    let mut broken = job;
    broken.args = json!({"steps": []});
    let outcome = f
        .handler
        .handle(&JobContext {
            attempt: 1,
            job: broken,
            clock: now(),
        })
        .await;
    assert_eq!(
        outcome,
        HandlerResult::Fail("committed_delivery.bad_args".to_owned())
    );
    assert!(f.calls().is_empty(), "no step ran for any of them");
}

impl Fx {
    /// One attempt as the worker makes it: the job claimed from the store, so the handler sees
    /// the claim of its own attempt.
    async fn claimed(&self) -> spot_jobs::store::ClaimedJob {
        self.store
            .claim(
                &["default".to_owned()],
                "w",
                Duration::from_secs(60),
                now(),
                1,
            )
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    async fn run(&self, job: &Job) -> HandlerResult {
        self.handler
            .handle(&JobContext {
                attempt: job.attempt,
                job: job.clone(),
                clock: now(),
            })
            .await
    }
}

#[tokio::test]
async fn a_restart_between_a_step_and_its_marker_repeats_that_step_once_and_never_after_its_marker()
{
    // The markers sit on the store the jobs live in: build the fixture, then swap them in.
    let (probe, _) = fx_with(None).await;
    let durable: Arc<dyn StepMarkers> = Arc::new(JobStoreMarkers::new(probe.store.clone()));
    let crashing: Arc<dyn StepMarkers> = Arc::new(CrashBeforeMark {
        inner: durable.clone(),
        crashed: Mutex::new(false),
    });
    let f = rebuilt(probe, crashing);
    f.create("Acme").await;
    let id = f.intake().await[0];
    let claimed = f.claimed().await;
    assert_eq!(claimed.job.id, id);
    // The crash: step one ran, its marker did not land.
    assert!(matches!(f.run(&claimed.job).await, HandlerResult::Retry(_)));
    assert_eq!(f.calls(), ["create_committed"]);
    assert!(durable.done(id).await.unwrap().is_empty());
    // The retry repeats step one once, then writes both markers.
    assert_eq!(f.run(&claimed.job).await, HandlerResult::Ok);
    assert_eq!(
        f.calls(),
        ["create_committed", "create_committed", "save_committed"]
    );
    // Both are in the job's own context row now, so a fresh handler repeats nothing.
    assert_eq!(durable.done(id).await.unwrap().len(), 2);
    let stored = f.job(id).await;
    assert_eq!(
        stored.context["committed_done"],
        json!(["create_committed", "save_committed"])
    );
    assert_eq!(f.run(&claimed.job).await, HandlerResult::Ok);
    assert_eq!(f.calls().len(), 3);
}

/// The fixture again, with `markers` in place of the ones it was built with and the same store.
fn rebuilt(old: Fx, markers: Arc<dyn StepMarkers>) -> Fx {
    let handler = Arc::new(CommittedStepsHandler::new(
        Arc::new(SingleSite::new(
            SiteId::new(SITE).unwrap(),
            Arc::new(old.engine.runner()),
        )),
        markers,
        old.store.clone(),
    ));
    Fx { handler, ..old }
}

#[tokio::test]
async fn a_lost_claim_writes_no_marker_and_the_job_retries() {
    let (probe, _) = fx_with(None).await;
    let durable: Arc<dyn StepMarkers> = Arc::new(JobStoreMarkers::new(probe.store.clone()));
    let f = rebuilt(probe, durable.clone());
    f.create("Acme").await;
    let id = f.intake().await[0];
    let claimed = f.claimed().await;
    // The lease is gone (the job was finished or reaped): the store refuses the marker.
    f.store
        .complete(&claimed.claim, JobState::Succeeded, None)
        .await
        .unwrap();
    assert!(matches!(f.run(&claimed.job).await, HandlerResult::Retry(_)));
    assert!(durable.done(id).await.unwrap().is_empty());
    // An attempt with no claim at all cannot record progress either.
    let mut unclaimed = claimed.job.clone();
    unclaimed.claim = None;
    assert!(matches!(f.run(&unclaimed).await, HandlerResult::Retry(_)));
    assert!(durable.done(id).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_corrupt_marker_list_is_an_error_never_an_empty_one() {
    let (probe, _) = fx_with(None).await;
    let durable = JobStoreMarkers::new(probe.store.clone());
    probe.create("Acme").await;
    let id = probe.intake().await[0];
    let claimed = probe.claimed().await;
    let mut patch = serde_json::Map::new();
    patch.insert("committed_done".to_owned(), json!("create_committed"));
    probe
        .store
        .merge_context(id, &claimed.claim, patch)
        .await
        .unwrap();
    assert_eq!(durable.done(id).await, Err(MarkerError));
    // An unknown job is an error as well.
    assert_eq!(durable.done(JobId(u64::MAX)).await, Err(MarkerError));
}
