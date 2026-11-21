//! Internal schedules (WP-BPM-11.T3): a service registers a schedule and its handler with the jobs
//! module; the scheduler plans it like a manifest schedule and the worker's registry runs it.
//!
//! The registry tests need no database. The scheduler tests run the real `Scheduling` service on
//! a manual clock over the in-memory job store and the in-process coordinator, and skip, printing
//! `SKIPPED:`, when `SPOT_TEST_PG_URL` is unset (the service's settings carry a database handle
//! the schedule path never uses).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use spot_app_api::StepName;
use spot_cli::services::jobs::{
    InternalHandler, InternalSchedule, InternalScheduleRegistry, InternalSchedules, Scheduling,
    Settings,
};
use spot_cli::services::{Level, Service};
use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use spot_jobs::store::{JobFilter, JobStore};
use spot_jobs::worker::{BoxFuture, HandlerResult, JobContext, JobHandlerRegistry};
use spot_jobs::{InMemoryJobStore, Job};
use spot_manifest::{Cron, Queue};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{MissedRunPolicy, Schedule, ScheduleBuilder, TimeZone};
use time::OffsetDateTime;
use time::macros::datetime;
use tokio::sync::watch;

const EVERY_MINUTE: &str = "0 * * * * *";

fn t0() -> OffsetDateTime {
    datetime!(2026-11-12 10:00:30 UTC)
}

fn counting(counter: &Arc<AtomicU32>) -> InternalHandler {
    let counter = Arc::clone(counter);
    Arc::new(
        move |_ctx: JobContext| -> BoxFuture<'static, HandlerResult> {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                HandlerResult::Ok
            })
        },
    )
}

fn schedule(
    service: &str,
    name: &str,
    policy: MissedRunPolicy,
    handler: InternalHandler,
) -> InternalSchedule {
    InternalSchedule {
        service: service.to_owned(),
        name: name.to_owned(),
        cron: EVERY_MINUTE.to_owned(),
        missed_run_policy: policy,
        queue: "default".to_owned(),
        budget: Duration::from_secs(30),
        handler,
    }
}

fn manifest_schedule(name: &str, step: &str) -> Arc<Schedule> {
    Arc::new(
        ScheduleBuilder::new(name)
            .cron(Cron::parse(EVERY_MINUTE).unwrap())
            .step(StepName::new(step).unwrap())
            .queue(Queue::Default)
            .zone(TimeZone::parse("UTC").unwrap())
            .build()
            .unwrap(),
    )
}

#[test]
fn a_schedule_registers_under_its_service_and_name() {
    let registry = InternalScheduleRegistry::new();
    let counter = Arc::new(AtomicU32::new(0));
    registry
        .register(schedule(
            "toy",
            "tick",
            MissedRunPolicy::RunOnce,
            counting(&counter),
        ))
        .unwrap();
    assert_eq!(registry.names(), vec!["toy:tick".to_owned()]);

    let mut handlers = JobHandlerRegistry::new();
    let planned = registry.install(&[], &mut handlers).unwrap();
    assert_eq!(planned.len(), 1);
    assert_eq!(
        planned[0].0, "toy",
        "the service is the app part of the job name"
    );
    assert_eq!(planned[0].2, MissedRunPolicy::RunOnce);
    assert!(
        handlers.get("toy:tick").is_some(),
        "the handler is in the worker's registry"
    );
}

#[test]
fn malformed_registrations_are_refused() {
    let registry = InternalScheduleRegistry::new();
    let counter = Arc::new(AtomicU32::new(0));
    let handler = counting(&counter);
    assert!(
        registry
            .register(schedule(
                "Bad Service",
                "tick",
                MissedRunPolicy::Skip,
                Arc::clone(&handler)
            ))
            .is_err()
    );
    assert!(
        registry
            .register(schedule(
                "toy",
                "not a name",
                MissedRunPolicy::Skip,
                Arc::clone(&handler)
            ))
            .is_err()
    );
    let mut bad_cron = schedule("toy", "tick", MissedRunPolicy::Skip, Arc::clone(&handler));
    bad_cron.cron = "every minute".to_owned();
    assert!(registry.register(bad_cron).is_err());
    let mut bad_queue = schedule("toy", "tick", MissedRunPolicy::Skip, Arc::clone(&handler));
    bad_queue.queue = "urgent".to_owned();
    assert!(registry.register(bad_queue).is_err());
    assert!(registry.names().is_empty());

    registry
        .register(schedule(
            "toy",
            "tick",
            MissedRunPolicy::Skip,
            Arc::clone(&handler),
        ))
        .unwrap();
    assert!(
        registry
            .register(schedule("toy", "tick", MissedRunPolicy::Skip, handler))
            .is_err(),
        "the same service and name twice"
    );
}

#[test]
fn a_registration_after_install_is_refused() {
    let registry = InternalScheduleRegistry::new();
    let counter = Arc::new(AtomicU32::new(0));
    let mut handlers = JobHandlerRegistry::new();
    registry.install(&[], &mut handlers).unwrap();
    assert!(
        registry
            .register(schedule(
                "toy",
                "late",
                MissedRunPolicy::Skip,
                counting(&counter)
            ))
            .is_err()
    );
}

#[test]
fn a_clash_with_a_manifest_schedule_or_a_handler_stops_the_start() {
    let counter = Arc::new(AtomicU32::new(0));

    // A manifest schedule whose step is the internal job's name.
    let registry = InternalScheduleRegistry::new();
    registry
        .register(schedule(
            "toy",
            "tick",
            MissedRunPolicy::Skip,
            counting(&counter),
        ))
        .unwrap();
    let manifest = vec![(
        manifest_schedule("nightly", "toy:tick"),
        MissedRunPolicy::Skip,
    )];
    let error = registry
        .install(&manifest, &mut JobHandlerRegistry::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("toy:tick"), "{error}");

    // A handler that is already registered under the job's name.
    let registry = InternalScheduleRegistry::new();
    registry
        .register(schedule(
            "toy",
            "tick",
            MissedRunPolicy::Skip,
            counting(&counter),
        ))
        .unwrap();
    let mut handlers = JobHandlerRegistry::new();
    handlers.register("toy:tick", |_ctx: JobContext| {
        Box::pin(async { Ok(HandlerResult::Ok) })
    });
    assert!(registry.install(&[], &mut handlers).is_err());

    // No clash: a manifest schedule of another name.
    let registry = InternalScheduleRegistry::new();
    registry
        .register(schedule(
            "toy",
            "tick",
            MissedRunPolicy::Skip,
            counting(&counter),
        ))
        .unwrap();
    let manifest = vec![(
        manifest_schedule("nightly", "sample:nightly"),
        MissedRunPolicy::Skip,
    )];
    assert!(
        registry
            .install(&manifest, &mut JobHandlerRegistry::new())
            .is_ok()
    );
}

#[test]
fn two_sites_have_registries_that_share_nothing() {
    let (a, b) = (
        InternalScheduleRegistry::new(),
        InternalScheduleRegistry::new(),
    );
    let counter = Arc::new(AtomicU32::new(0));
    a.register(schedule(
        "toy",
        "tick",
        MissedRunPolicy::Skip,
        counting(&counter),
    ))
    .unwrap();
    assert!(b.names().is_empty());
    let mut handlers_b = JobHandlerRegistry::new();
    assert!(b.install(&[], &mut handlers_b).unwrap().is_empty());
    assert!(
        handlers_b.get("toy:tick").is_none(),
        "site B has no handler for site A's job"
    );
}

// ---- the scheduler ----

macro_rules! db {
    () => {
        match TestDb::create().await {
            Some(db) => db,
            None => {
                println!("SKIPPED: SPOT_TEST_PG_URL is not set");
                return;
            }
        }
    };
}

fn backend(db: &TestDb) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = "internal_schedules".to_owned();
    cfg.pool_max_size = 2;
    PgBackend::connect_unchecked(cfg).unwrap()
}

/// One site's scheduler on a manual clock. The coordinator keeps real time, so the lease does not
/// lapse when the manual clock jumps; the lease lasts an hour.
struct Rig {
    clock: Arc<ManualClock>,
    store: Arc<InMemoryJobStore>,
    coordinator: Arc<dyn Coordinator>,
    site: SiteId,
}

impl Rig {
    async fn new() -> Rig {
        let coordinator = InProcessCoordinator::start(
            Arc::new(InMemoryPresence::new()),
            NodeId::new("rig".to_owned()).unwrap(),
            Arc::new(SystemClock::new()),
        )
        .await
        .unwrap();
        Rig {
            clock: Arc::new(ManualClock::new(t0())),
            store: Arc::new(InMemoryJobStore::new()),
            coordinator,
            site: SiteId::new("acme").unwrap(),
        }
    }

    fn settings(&self, db: &TestDb, node: &str) -> Settings {
        Settings {
            site: self.site.clone(),
            backend: backend(db),
            store: Arc::clone(&self.store) as Arc<dyn JobStore>,
            queues: vec!["default".to_owned()],
            intake_interval: Duration::from_millis(100),
            grace: Duration::from_millis(100),
            node: node.to_owned(),
            clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
            coordinator: Arc::clone(&self.coordinator),
            consumer_ttl: Duration::from_secs(3600),
            schedules: Vec::new(),
            tick_interval: Duration::from_millis(20),
            lease_ttl: Duration::from_secs(3600),
        }
    }

    /// Starts a scheduler node planning `planned`; returns its stop sender and task.
    fn start(
        &self,
        db: &TestDb,
        node: &str,
        planned: Vec<(String, Arc<Schedule>, MissedRunPolicy)>,
    ) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let service = Scheduling::with_internal(&self.settings(db, node), planned);
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _ = service.run(stopped).await;
        });
        (stop, task)
    }

    async fn jobs(&self) -> Vec<Job> {
        // The jobs of the schedules under test: the scheduler also plans the host's own schedules.
        self.store
            .list(JobFilter {
                site: Some(self.site.clone()),
                ..JobFilter::default()
            })
            .await
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("toy:"))
            .collect()
    }

    /// Waits until `want` jobs exist (up to five seconds) and returns them.
    async fn wait_for(&self, want: usize) -> Vec<Job> {
        for _ in 0..250 {
            let jobs = self.jobs().await;
            if jobs.len() >= want {
                return jobs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.jobs().await
    }
}

fn planned_of(
    policy: MissedRunPolicy,
    counter: &Arc<AtomicU32>,
) -> (
    Vec<(String, Arc<Schedule>, MissedRunPolicy)>,
    JobHandlerRegistry,
) {
    let registry = InternalScheduleRegistry::new();
    registry
        .register(schedule("toy", "tick", policy, counting(counter)))
        .unwrap();
    let mut handlers = JobHandlerRegistry::new();
    let planned = registry.install(&[], &mut handlers).unwrap();
    (planned, handlers)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_schedule_fires_on_a_controllable_clock_and_runs_its_handler_once_under_the_lease() {
    let db = db!();
    let rig = Rig::new().await;
    let counter = Arc::new(AtomicU32::new(0));
    let (planned, handlers) = planned_of(MissedRunPolicy::RunOnce, &counter);
    // Two nodes on one site and one coordinator: only the lease holder plans.
    let (stop_a, task_a) = rig.start(&db, "node-a", planned.clone());
    let (stop_b, task_b) = rig.start(&db, "node-b", planned);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        rig.jobs().await.is_empty(),
        "nothing is due before the clock moves"
    );

    rig.clock.advance(Duration::from_secs(61));
    let jobs = rig.wait_for(1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let jobs_after = rig.jobs().await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs_after.len(),
        1,
        "the second node did not enqueue it again"
    );
    assert_eq!(jobs_after[0].name, "toy:tick");
    assert_eq!(jobs_after[0].queue, "default");

    // The worker's registry runs the handler for that job, once.
    let handler = handlers
        .get("toy:tick")
        .expect("registered under the job name");
    let result = (**handler)(JobContext::new(
        jobs_after[0].clone(),
        1,
        rig.clock.now_utc(),
    ))
    .await
    .unwrap();
    assert!(matches!(result, HandlerResult::Ok));
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    let _ = (stop_a.send(true), stop_b.send(true));
    let _ = (task_a.await, task_b.await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missed_runs_coalesce_into_one_under_run_once() {
    let db = db!();
    let rig = Rig::new().await;
    let counter = Arc::new(AtomicU32::new(0));
    let (planned, _handlers) = planned_of(MissedRunPolicy::RunOnce, &counter);
    let (stop, task) = rig.start(&db, "node-a", planned);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Five minutes pass with nobody planning: five firings are due. The grace is a minute
    // (`schedule_grace`), so the 10:05 firing is on time and runs by itself; the four older ones
    // are missed and coalesce into one run.
    rig.clock.advance(Duration::from_secs(300));
    let jobs = rig.wait_for(2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(jobs.len(), 2);
    let all = rig.jobs().await;
    assert_eq!(
        all.len(),
        2,
        "the missed runs coalesce into one, beside the one on time"
    );
    let mut keys: Vec<String> = all
        .iter()
        .map(|j| j.idempotency_key.clone().unwrap_or_default())
        .collect();
    keys.sort();
    assert!(keys[0].ends_with("2026-11-12T10:04:00Z"), "{keys:?}");
    assert!(keys[1].ends_with("2026-11-12T10:05:00Z"), "{keys:?}");

    let _ = stop.send(true);
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_skip_policy_enqueues_nothing_for_a_missed_window() {
    let db = db!();
    let rig = Rig::new().await;
    let counter = Arc::new(AtomicU32::new(0));
    let (planned, _handlers) = planned_of(MissedRunPolicy::Skip, &counter);
    let (stop, task) = rig.start(&db, "node-a", planned);
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.clock.advance(Duration::from_secs(300));
    // The firing a tick reaches within the grace (10:05, thirty seconds old) runs; the four
    // missed ones before it are dropped, not replayed.
    let jobs = rig.wait_for(1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let all = rig.jobs().await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(all.len(), 1, "skip plans nothing for the missed window");
    let key = all[0].idempotency_key.clone().unwrap_or_default();
    assert!(key.ends_with("2026-11-12T10:05:00Z"), "{key}");

    let _ = stop.send(true);
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scheduler_of_site_a_never_enqueues_in_site_b() {
    let db = db!();
    let (a, b) = (Rig::new().await, Rig::new().await);
    let counter = Arc::new(AtomicU32::new(0));
    let (planned, _handlers) = planned_of(MissedRunPolicy::RunOnce, &counter);
    let (stop, task) = a.start(&db, "node-a", planned);
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.clock.advance(Duration::from_secs(61));
    assert_eq!(a.wait_for(1).await.len(), 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        b.jobs().await.is_empty(),
        "site B's store holds nothing of site A's schedule"
    );

    let _ = stop.send(true);
    let _ = task.await;
}

#[test]
fn the_scheduler_is_still_a_required_service() {
    // A schedule that never fires is a silent data problem: internal schedules do not change that.
    // (Compile-time check that `Level` and `Service` are the host's own.)
    let _: fn(&Scheduling) -> Level = |s| s.level();
}
