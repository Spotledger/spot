//! The committed-steps job as the host hosts it (WP-DOC-04.T9): the handler is registered by
//! `install_committed_steps`, the way `services::jobs::register` does, with its done markers in
//! the job's own context row of the store the worker claims from (never `InMemoryMarkers`).
//!
//! - end to end on the in-memory job store: a document whose second committed step fails once
//!   and then succeeds; the retry runs the second step only, and the first step's effect is 1;
//! - the same case on a real PostgreSQL job store and document engine (`SPOT_TEST_PG_URL`; a
//!   test prints `SKIPPED:` and returns without it, as the other PostgreSQL tests do);
//! - restart on PostgreSQL: the worker is dropped while the second step runs (after the first
//!   marker was written, before the second could be); a fresh worker on the same database
//!   repeats the second step at most once and never the first, and the finished job lists every
//!   step in `committed_done`;
//! - the live runner answers a retryable error while the site has no engine yet, and refuses a
//!   site it does not serve.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, Document, ErrorClass, HookPoint, Phase, Target,
};
use spot_cli::live::SiteLive;
use spot_cli::services::jobs::{LiveRunners, install_committed_steps};
use spot_committed_delivery::{RunnerFor, SingleSite};
use spot_core::{ManualClock, SiteId, SystemClock};
use spot_document::engine::{
    DurableDelivery, Engine, JOB_COMMITTED_STEPS, RetryPolicy, V7Ids, job_from_event,
};
use spot_events_contract::lifecycle::Operation;
use spot_jobs::store::JobStore;
use spot_jobs::worker::{HandlerResult, JobContext, JobHandlerRegistry};
use spot_jobs::{InMemoryJobStore, JobId, NewJob};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, FeedPosition, StorageBackend};
use spot_storage_fake::FakeBackend;
use tokio::sync::Notify;

const SITE: &str = "markers.test";
const QUEUE: &str = "default";
const FIRST: &str = "create_committed";
const SECOND: &str = "save_committed";

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn fixed_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap()
}

#[derive(Clone)]
enum Behaviour {
    Ok,
    /// A temporary infrastructure error on the first call only.
    UnavailableOnce,
    /// The step starts and never finishes (the worker is dropped meanwhile).
    Hang(Arc<Notify>),
}

#[derive(Default)]
struct Script {
    calls: Vec<String>,
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
        budget: Duration::from_secs(30),
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
        ]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        let point = ctx.point().name();
        let behaviour = {
            let mut script = self.script.lock().unwrap();
            script.calls.push(point.clone());
            let behaviour = script
                .behaviour
                .get(&point)
                .cloned()
                .unwrap_or(Behaviour::Ok);
            if matches!(behaviour, Behaviour::UnavailableOnce) {
                if script.failed_once.contains(&point) {
                    Behaviour::Ok
                } else {
                    script.failed_once.push(point);
                    Behaviour::UnavailableOnce
                }
            } else {
                behaviour
            }
        };
        Box::pin(async move {
            match behaviour {
                Behaviour::Ok => Ok(()),
                Behaviour::UnavailableOnce => {
                    Err(AppError::new(ErrorClass::Unavailable, "probe.down", "down"))
                }
                Behaviour::Hang(gate) => {
                    gate.notified().await;
                    Ok(())
                }
            }
        })
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

fn invocation() -> Invocation {
    Invocation {
        site: spot_app_api::prelude::SiteId::new(SITE).unwrap(),
        actor: Actor::System {
            component: n("committed_markers_tests"),
        },
        command_id: CommandId::new("cmd-9").ok(),
        language: None,
        now: fixed_now(),
    }
}

fn site() -> SiteId {
    SiteId::new(SITE).unwrap()
}

fn count(script: &Shared, point: &str) -> usize {
    script
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.as_str() == point)
        .count()
}

fn set(script: &Shared, point: &str, behaviour: Behaviour) {
    script
        .lock()
        .unwrap()
        .behaviour
        .insert(point.to_owned(), behaviour);
}

fn build_engine<B: StorageBackend>(
    backend: B,
    script: &Shared,
    clock: Arc<dyn spot_core::Clock>,
) -> Engine<B> {
    let mut builder = RegistryBuilder::new();
    builder.add_controller(
        "probe",
        Arc::new(Probe {
            script: script.clone(),
        }),
    );
    let meta = Registry::new(definitions()).unwrap().effective();
    Engine::new(
        backend,
        meta,
        Arc::new(builder.build().unwrap()),
        clock,
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(RetryPolicy {
        max_retries: 1,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    })
    .with_delivery(Arc::new(DurableDelivery::new(Arc::new(V7Ids))))
}

async fn create_customer<B: StorageBackend>(engine: &Engine<B>, name: &str) {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    engine.create(&invocation(), doc).await.unwrap();
}

/// What the host's intake does with the outbox: each job event becomes a stored job.
async fn intake<B: StorageBackend + ChangeFeed>(
    engine: &Engine<B>,
    store: &dyn JobStore,
    now: OffsetDateTime,
) -> Vec<JobId> {
    let events = engine
        .backend()
        .read_outbox(FeedPosition::START, 10_000)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for record in events {
        let Some(request) = job_from_event(&record.event).unwrap() else {
            continue;
        };
        let mut job = NewJob::new(request.job, request.args, QUEUE.to_owned(), site(), now)
            .with_outbox_event_id(record.event.id.to_string());
        if let Some(key) = request.idempotency_key {
            job = job.with_idempotency_key(key);
        }
        match store.insert_if_new(job).await.unwrap() {
            spot_jobs::model::InsertResult::Inserted(id)
            | spot_jobs::model::InsertResult::Duplicate(id) => ids.push(id),
        }
    }
    ids
}

/// The handler as the host registers it, resolved through the registry like the worker does.
async fn attempt(registry: &JobHandlerRegistry, ctx: JobContext) -> HandlerResult {
    let handler = registry
        .get(JOB_COMMITTED_STEPS)
        .expect("the handler is registered");
    (*handler)(ctx).await.unwrap()
}

async fn done_points(store: &dyn JobStore, id: JobId) -> Vec<String> {
    let stored = store.get(id).await.unwrap().unwrap();
    stored.context["committed_done"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The shared case: the second step fails once; the retry runs it alone.
async fn second_step_fails_once_then_succeeds<B: StorageBackend + ChangeFeed>(
    engine: Arc<Engine<B>>,
    script: Shared,
    store: Arc<dyn JobStore>,
    now: OffsetDateTime,
) {
    set(&script, SECOND, Behaviour::UnavailableOnce);
    create_customer(&engine, "Acme").await;
    assert_eq!(count(&script, FIRST), 0, "nothing ran at the commit");
    let ids = intake(&engine, &*store, now).await;
    assert_eq!(ids.len(), 1);

    let mut registry = JobHandlerRegistry::new();
    install_committed_steps(
        &mut registry,
        Arc::new(SingleSite::new(site(), Arc::new(engine.runner()))),
        Arc::clone(&store),
    );

    let claimed = store
        .claim(&[QUEUE.to_owned()], "w1", Duration::from_secs(120), now, 1)
        .await
        .unwrap();
    let job = claimed.into_iter().next().unwrap().job;
    assert!(job.claim.is_some(), "the worker's attempt holds its claim");

    let first = attempt(&registry, JobContext::new(job.clone(), 1, now)).await;
    assert!(matches!(first, HandlerResult::Retry(_)), "{first:?}");
    assert_eq!(done_points(&*store, ids[0]).await, [FIRST]);

    let second = attempt(&registry, JobContext::new(job.clone(), 2, now)).await;
    assert_eq!(second, HandlerResult::Ok);
    assert_eq!(count(&script, FIRST), 1, "the first step's effect is 1");
    assert_eq!(
        count(&script, SECOND),
        2,
        "the second step ran twice: fail, then success"
    );
    assert_eq!(done_points(&*store, ids[0]).await, [FIRST, SECOND]);

    // A repeat of the finished job runs nothing.
    assert_eq!(
        attempt(&registry, JobContext::new(job, 3, now)).await,
        HandlerResult::Ok
    );
    assert_eq!(count(&script, FIRST), 1);
    assert_eq!(count(&script, SECOND), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_memory_store_a_failed_second_step_resumes_at_the_second_step_only() {
    let script: Shared = Arc::default();
    let engine = build_engine(
        FakeBackend::new(),
        &script,
        Arc::new(ManualClock::new(fixed_now())),
    );
    engine.prepare().await.unwrap();
    second_step_fails_once_then_succeeds(
        Arc::new(engine),
        script,
        Arc::new(InMemoryJobStore::new()),
        fixed_now(),
    )
    .await;
}

fn pg_backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 4;
    PgBackend::connect_unchecked(cfg).unwrap()
}

async fn pg_store(backend: &PgBackend) -> Arc<dyn JobStore> {
    let store = spot_jobs_pg::PgJobStore::new(backend.pool());
    store.prepare().await.unwrap();
    Arc::new(store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_store_a_failed_second_step_resumes_at_the_second_step_only() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let script: Shared = Arc::default();
    let backend = pg_backend(&db, "committed_markers_a");
    let engine = build_engine(backend.clone(), &script, Arc::new(SystemClock::new()));
    engine.prepare().await.unwrap();
    let store = pg_store(&backend).await;
    second_step_fails_once_then_succeeds(
        Arc::new(engine),
        script,
        store,
        OffsetDateTime::now_utc(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_restart_between_a_step_and_its_marker_repeats_at_most_that_step() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let script: Shared = Arc::default();
    let now = OffsetDateTime::now_utc();
    let backend = pg_backend(&db, "committed_markers_b");
    let engine = Arc::new(build_engine(
        backend.clone(),
        &script,
        Arc::new(SystemClock::new()),
    ));
    engine.prepare().await.unwrap();
    let store = pg_store(&backend).await;
    create_customer(&engine, "Beta").await;
    let ids = intake(&engine, &*store, now).await;
    assert_eq!(ids.len(), 1);
    let id = ids[0];

    // Worker one: the second step starts and never ends; the worker is dropped meanwhile.
    set(&script, SECOND, Behaviour::Hang(Arc::new(Notify::new())));
    let mut registry = JobHandlerRegistry::new();
    install_committed_steps(
        &mut registry,
        Arc::new(SingleSite::new(site(), Arc::new(engine.runner()))),
        Arc::clone(&store),
    );
    let claimed = store
        .claim(&[QUEUE.to_owned()], "w1", Duration::from_secs(120), now, 1)
        .await
        .unwrap();
    let job = claimed.into_iter().next().unwrap().job;
    let handler = registry.get(JOB_COMMITTED_STEPS).unwrap();
    let running = tokio::spawn({
        let ctx = JobContext::new(job.clone(), 1, now);
        async move { (*handler)(ctx).await }
    });
    for _ in 0..200 {
        if count(&script, SECOND) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(count(&script, SECOND), 1, "the second step started");
    running.abort();
    let _ = running.await;
    drop(registry);
    assert_eq!(
        done_points(&*store, id).await,
        [FIRST],
        "only the first marker exists"
    );

    // Worker two: a fresh handler over a fresh store object on the same database.
    set(&script, SECOND, Behaviour::Ok);
    let fresh_backend = pg_backend(&db, "committed_markers_c");
    let fresh_store = pg_store(&fresh_backend).await;
    let mut registry = JobHandlerRegistry::new();
    install_committed_steps(
        &mut registry,
        Arc::new(SingleSite::new(site(), Arc::new(engine.runner()))),
        Arc::clone(&fresh_store),
    );
    let result = attempt(&registry, JobContext::new(job, 2, now)).await;
    assert_eq!(result, HandlerResult::Ok);
    assert_eq!(count(&script, FIRST), 1, "a marked step is never repeated");
    assert_eq!(
        count(&script, SECOND),
        2,
        "the interrupted step ran again once, never after its marker"
    );
    assert_eq!(done_points(&*fresh_store, id).await, [FIRST, SECOND]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_runner_serves_only_its_site_and_is_retryable_before_the_engine_exists() {
    let live = Arc::new(SiteLive::new());
    let runners = LiveRunners::new(site(), live);
    assert!(
        runners
            .runner(&SiteId::new("other.test").unwrap())
            .is_none()
    );
    let runner = runners.runner(&site()).expect("its own site");
    // No engine is running: reading a document is a retryable error, never a missing document.
    let error = runner
        .current("customer", RecordId::new_v7())
        .await
        .unwrap_err();
    assert!(error.retryable(), "{error:?}");
}
