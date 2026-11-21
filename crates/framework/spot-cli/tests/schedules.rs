//! Schedules in `spot serve` (WP-PLT-07.T3): a `[[schedules]]` entry of an app, and of the platform
//! app, produces exactly one job per firing, named `<app>:<schedule>`, through a restart of the
//! leader, a lease failover and a failed insert.
//!
//! No test needs a database: the loop under test is the real [`ScheduleLoop`] over the in-memory
//! job store and the in-process coordinator, on a manual clock the test advances. The leader
//! lease keeps real time in the coordinator, so the lease is short (300 ms) and an aborted node's
//! lease lapses by itself.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use jiff::Timestamp;
use spot_app_api::StepName;
use spot_cli::services::jobs::{
    HostedSchedule, ScheduleLoop, ScheduleLoopSettings, SlotOutcome, embedded_platform_manifest,
    enqueue_slot, hosted_schedules, schedule_grace,
};
use spot_cli::services::schedule_marks::CoordinatorMarks;
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, Error as CoordError, FencingToken, InMemoryPresence,
    InProcessCoordinator, Lease, NodeId, RateLimit, Result as CoordResult,
};
use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use spot_jobs::model::{Claim, InsertResult};
use spot_jobs::store::{ClaimedJob, JobFilter, JobStore};
use spot_jobs::{InMemoryJobStore, Job, JobId, JobState, NewJob};
use spot_manifest::{Cron, Queue};
use spot_scheduler::{MissedRunPolicy, RunSlot, Schedule, ScheduleBuilder, SlotRef, TimeZone};
use spot_storage_contract::feed::FeedPosition;
use time::OffsetDateTime;
use time::macros::datetime;
use tokio::sync::{Notify, watch};

const HOURLY: &str = "0 0 * * * *";
const LEASE: Duration = Duration::from_millis(300);
const TICK: Duration = Duration::from_millis(20);

fn t0() -> OffsetDateTime {
    datetime!(2026-11-12 00:00:00 UTC)
}

fn hourly(name: &str, step: &str) -> Arc<Schedule> {
    Arc::new(
        ScheduleBuilder::new(name)
            .cron(Cron::parse(HOURLY).unwrap())
            .step(StepName::new(step).unwrap())
            .queue(Queue::Default)
            .budget(Duration::from_secs(90))
            .zone(TimeZone::parse("UTC").unwrap())
            .build()
            .unwrap(),
    )
}

fn hosted(app: &str, name: &str, step: &str, policy: MissedRunPolicy) -> HostedSchedule {
    HostedSchedule {
        app: app.to_owned(),
        schedule: hourly(name, step),
        policy,
    }
}

// ---- doubles ----

/// A coordinator whose `peek` can be made to fail; everything else is the real one.
struct FlakyCoordinator {
    inner: Arc<dyn Coordinator>,
    peek_down: AtomicBool,
}

impl Coordinator for FlakyCoordinator {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, CoordResult<Lease>> {
        self.inner.acquire(name, holder, ttl)
    }
    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, CoordResult<Lease>> {
        self.inner.renew(lease, ttl)
    }
    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, CoordResult<()>> {
        self.inner.release(lease)
    }
    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<Option<Lease>>> {
        self.inner.current(name)
    }
    fn check_fence<'a>(
        &'a self,
        name: &'a str,
        token: FencingToken,
    ) -> BoxFuture<'a, CoordResult<()>> {
        self.inner.check_fence(name, token)
    }
    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.next(name, by)
    }
    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<u64>> {
        if self.peek_down.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(CoordError::new(
                    "coordinator.unavailable",
                    "the coordinator is down (test)",
                ))
            });
        }
        self.inner.peek(name)
    }
    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.set_at_least(name, value)
    }
    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, CoordResult<Admit>> {
        self.inner.take(key, limit, cost)
    }
    fn waker(&self) -> Arc<Notify> {
        self.inner.waker()
    }
    fn release_now(&self, lease: &Lease) -> bool {
        self.inner.release_now(lease)
    }
}

type StoreFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<T>> + Send + 'a>>;

/// A job store whose next `fail_inserts` inserts fail; everything else is the real one.
struct FlakyStore {
    inner: InMemoryJobStore,
    fail_inserts: AtomicU32,
}

impl JobStore for FlakyStore {
    fn insert_if_new(&self, job: NewJob) -> StoreFuture<'_, InsertResult> {
        let failing = self
            .fail_inserts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if failing {
            return Box::pin(async { Err(spot_jobs::Error::other("store unavailable (test)")) });
        }
        self.inner.insert_if_new(job)
    }
    fn claim(
        &self,
        queues: &[String],
        worker_id: &str,
        ttl: Duration,
        now: OffsetDateTime,
        limit: u32,
    ) -> StoreFuture<'_, Vec<ClaimedJob>> {
        self.inner.claim(queues, worker_id, ttl, now, limit)
    }
    fn heartbeat(&self, claim: &Claim, ttl: Duration, now: OffsetDateTime) -> StoreFuture<'_, ()> {
        self.inner.heartbeat(claim, ttl, now)
    }
    fn complete(
        &self,
        claim: &Claim,
        new_state: JobState,
        last_error: Option<String>,
    ) -> StoreFuture<'_, ()> {
        self.inner.complete(claim, new_state, last_error)
    }
    fn reap_expired(&self, now: OffsetDateTime) -> StoreFuture<'_, Vec<JobId>> {
        self.inner.reap_expired(now)
    }
    fn cancel(&self, id: JobId) -> StoreFuture<'_, ()> {
        self.inner.cancel(id)
    }
    fn cancel_if_pending(&self, id: JobId) -> StoreFuture<'_, spot_jobs::CancelOutcome> {
        self.inner.cancel_if_pending(id)
    }
    fn merge_context(
        &self,
        id: JobId,
        claim: &Claim,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> StoreFuture<'_, serde_json::Value> {
        self.inner.merge_context(id, claim, patch)
    }
    fn get(&self, id: JobId) -> StoreFuture<'_, Option<Job>> {
        self.inner.get(id)
    }
    fn list(&self, filter: JobFilter) -> StoreFuture<'_, Vec<Job>> {
        self.inner.list(filter)
    }
    fn intake_position(&self) -> StoreFuture<'_, FeedPosition> {
        self.inner.intake_position()
    }
    fn set_intake_position(&self, position: FeedPosition) -> StoreFuture<'_, ()> {
        self.inner.set_intake_position(position)
    }
}

// ---- the rig ----

struct Rig {
    clock: Arc<ManualClock>,
    store: Arc<FlakyStore>,
    coordinator: Arc<FlakyCoordinator>,
    site: SiteId,
}

impl Rig {
    async fn new() -> Rig {
        let real = InProcessCoordinator::start(
            Arc::new(InMemoryPresence::new()),
            NodeId::new("rig".to_owned()).unwrap(),
            Arc::new(SystemClock::new()),
        )
        .await
        .unwrap();
        Rig {
            clock: Arc::new(ManualClock::new(t0())),
            store: Arc::new(FlakyStore {
                inner: InMemoryJobStore::new(),
                fail_inserts: AtomicU32::new(0),
            }),
            coordinator: Arc::new(FlakyCoordinator {
                inner: real,
                peek_down: AtomicBool::new(false),
            }),
            site: SiteId::new("acme").unwrap(),
        }
    }

    fn schedule_loop(&self, node: &str, schedules: Vec<HostedSchedule>) -> Arc<ScheduleLoop> {
        ScheduleLoop::new(ScheduleLoopSettings {
            site: self.site.clone(),
            store: Arc::clone(&self.store) as Arc<dyn JobStore>,
            coordinator: Arc::clone(&self.coordinator) as Arc<dyn Coordinator>,
            clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
            schedules,
            tick_interval: TICK,
            lease_ttl: LEASE,
            node: node.to_owned(),
        })
    }

    /// Starts a node; returns its stop sender and task. Aborting the task is a crash: the lease
    /// is not released.
    fn start(
        &self,
        node: &str,
        schedules: Vec<HostedSchedule>,
    ) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let node = self.schedule_loop(node, schedules);
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _ = node.run(stopped).await;
        });
        (stop, task)
    }

    async fn jobs(&self) -> Vec<Job> {
        let mut jobs = self
            .store
            .list(JobFilter {
                site: Some(self.site.clone()),
                ..JobFilter::default()
            })
            .await
            .unwrap();
        jobs.sort_by(|a, b| (a.not_before, &a.name).cmp(&(b.not_before, &b.name)));
        jobs
    }

    /// Waits until `want` jobs exist (up to ten seconds) and returns them.
    async fn wait_for(&self, want: usize) -> Vec<Job> {
        for _ in 0..500 {
            let jobs = self.jobs().await;
            if jobs.len() >= want {
                return jobs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.jobs().await
    }

    /// Lets the loop tick a few times, then returns the jobs.
    async fn settle(&self) -> Vec<Job> {
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.jobs().await
    }

    fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }
}

fn keys(jobs: &[Job]) -> Vec<String> {
    jobs.iter()
        .map(|j| j.idempotency_key.clone().unwrap_or_default())
        .collect()
}

// ---- identity: <app>:<schedule> ----

#[tokio::test]
async fn an_hourly_schedule_gives_one_named_job_per_firing_over_26_hours() {
    let rig = Rig::new().await;
    let (stop, task) = rig.start(
        "node-a",
        vec![hosted("acme", "evict", "acme:evict", MissedRunPolicy::Skip)],
    );
    // The first firing is the one at the start instant; then one hour at a time.
    assert_eq!(rig.wait_for(1).await.len(), 1);
    for hour in 1..26_usize {
        rig.advance(Duration::from_secs(3600));
        assert_eq!(rig.wait_for(hour + 1).await.len(), hour + 1);
    }
    let jobs = rig.settle().await;
    assert_eq!(jobs.len(), 26, "one job per firing, none twice");
    assert!(jobs.iter().all(|j| j.name == "acme:evict"), "{jobs:?}");
    let keys = keys(&jobs);
    assert!(
        keys.iter().all(|k| k.starts_with("acme:evict:")),
        "{keys:?}"
    );
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 26, "every key is distinct");
    assert!(keys[0].ends_with("2026-11-12T00:00:00Z"), "{}", keys[0]);
    for job in &jobs {
        assert_eq!(job.args["step"], "acme:evict");
        assert_eq!(job.args["budget_ms"], 90_000);
        assert_eq!(job.queue, "default");
    }
    let _ = stop.send(true);
    let _ = task.await;
}

#[tokio::test]
async fn the_same_schedule_name_in_two_apps_gives_two_jobs_per_firing() {
    let rig = Rig::new().await;
    let (stop, task) = rig.start(
        "node-a",
        vec![
            hosted("acme", "nightly", "acme:nightly", MissedRunPolicy::Skip),
            hosted("beta", "nightly", "beta:nightly", MissedRunPolicy::Skip),
        ],
    );
    assert_eq!(rig.wait_for(2).await.len(), 2);
    for hour in 1..3_usize {
        rig.advance(Duration::from_secs(3600));
        assert_eq!(rig.wait_for(2 * (hour + 1)).await.len(), 2 * (hour + 1));
    }
    let jobs = rig.settle().await;
    assert_eq!(jobs.len(), 6, "no deduplication between the two apps");
    let acme = jobs.iter().filter(|j| j.name == "acme:nightly").count();
    let beta = jobs.iter().filter(|j| j.name == "beta:nightly").count();
    assert_eq!((acme, beta), (3, 3));
    let keys = keys(&jobs);
    assert!(keys.iter().any(|k| k.starts_with("acme:nightly:")));
    assert!(keys.iter().any(|k| k.starts_with("beta:nightly:")));
    let _ = stop.send(true);
    let _ = task.await;
}

#[test]
fn the_platform_manifests_schedules_are_tagged_platform_and_never_doubled() {
    let embedded = embedded_platform_manifest().unwrap();
    let declared: Vec<String> = embedded.schedules.iter().map(|s| s.name.clone()).collect();

    // With no catalogue entry the embedded manifest is the only source.
    let alone = hosted_schedules(&[(
        "platform".to_owned(),
        embedded.clone(),
        "the embedded platform app".to_owned(),
    )])
    .unwrap();
    let names: Vec<String> = alone.iter().map(|h| h.schedule.name.clone()).collect();
    assert_eq!(names, declared, "every declared schedule, in order");
    assert!(alone.iter().all(|h| h.app == "platform"));

    // A manifest that declares one more, listed twice (a catalogue entry and the embedded copy):
    // each schedule appears once.
    let mut with_extra = embedded;
    with_extra.schedules.push(spot_manifest::ScheduleSub {
        name: "test_extra".to_owned(),
        cron: Cron::parse(HOURLY).unwrap(),
        step: StepName::new("platform:test_extra").unwrap(),
        queue: Queue::Default,
        budget: Duration::from_secs(30),
    });
    let twice = hosted_schedules(&[
        (
            "platform".to_owned(),
            with_extra.clone(),
            "catalogue".to_owned(),
        ),
        (
            "platform".to_owned(),
            with_extra.clone(),
            "embedded".to_owned(),
        ),
    ])
    .unwrap();
    assert_eq!(twice.len(), with_extra.schedules.len());
    let extra = twice
        .iter()
        .filter(|h| h.schedule.name == "test_extra")
        .count();
    assert_eq!(extra, 1);
    assert!(
        twice
            .iter()
            .all(|h| h.policy == MissedRunPolicy::Skip && h.app == "platform")
    );
}

#[test]
fn the_grace_is_three_ticks_and_never_under_a_minute() {
    assert_eq!(
        schedule_grace(Duration::from_millis(20)),
        Duration::from_secs(60)
    );
    assert_eq!(
        schedule_grace(Duration::from_secs(30)),
        Duration::from_secs(90)
    );
}

// ---- marks ----

#[tokio::test]
async fn a_stored_mark_is_never_lowered_and_two_sites_never_share_one() {
    let rig = Rig::new().await;
    let coordinator = Arc::clone(&rig.coordinator) as Arc<dyn Coordinator>;
    let acme = CoordinatorMarks::new(Arc::clone(&coordinator), "acme");
    let other = CoordinatorMarks::new(coordinator, "other");
    assert_eq!(acme.load("app", "s").await.unwrap(), None);
    let two: Timestamp = "2026-11-12T02:00:00Z".parse().unwrap();
    let one: Timestamp = "2026-11-12T01:00:00Z".parse().unwrap();
    acme.store("app", "s", two).await.unwrap();
    // A node whose clock runs behind stores an older mark: nothing moves.
    acme.store("app", "s", one).await.unwrap();
    assert_eq!(acme.load("app", "s").await.unwrap(), Some(two));
    assert_eq!(other.load("app", "s").await.unwrap(), None);
    assert_eq!(
        acme.counter_name("app", "s"),
        "schedule_mark:acme:app:s".to_owned()
    );
}

// ---- failover ----

/// Node A leads at 00:00, ticks at 00:10 and crashes; node B starts at 02:10.
async fn crash_then_take_over(policy: MissedRunPolicy) -> Rig {
    let rig = Rig::new().await;
    let schedules = || vec![hosted("acme", "evict", "acme:evict", policy)];
    let (stop_a, task_a) = rig.start("node-a", schedules());
    assert_eq!(rig.wait_for(1).await.len(), 1);
    rig.advance(Duration::from_secs(600));
    assert_eq!(rig.settle().await.len(), 1, "nothing is due at 00:10");
    // A crash: the loop dies and its lease is not released.
    task_a.abort();
    let _ = task_a.await;
    drop(stop_a);
    rig.advance(Duration::from_secs(2 * 3600));
    let (stop_b, task_b) = rig.start("node-b", schedules());
    // Leaked on purpose: the test ends the runtime.
    std::mem::forget(stop_b);
    std::mem::forget(task_b);
    rig
}

#[tokio::test]
async fn a_new_leader_under_run_once_resumes_from_the_stored_mark() {
    let rig = crash_then_take_over(MissedRunPolicy::RunOnce).await;
    let jobs = rig.wait_for(2).await;
    let jobs_later = rig.settle().await;
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs_later.len(), 2, "exactly one job for the gap");
    let keys = keys(&jobs_later);
    assert!(keys[0].ends_with("2026-11-12T00:00:00Z"), "{keys:?}");
    assert!(keys[1].ends_with("2026-11-12T02:00:00Z"), "{keys:?}");
    assert!(
        !keys.iter().any(|k| k.ends_with("T01:00:00Z")),
        "the 01:00 firing is folded into the one run: {keys:?}"
    );
}

#[tokio::test]
async fn a_new_leader_under_skip_drops_the_gap_and_runs_the_next_firing() {
    let rig = crash_then_take_over(MissedRunPolicy::Skip).await;
    // B is leading by now (lease expiry plus a standby tick); the gap produces nothing.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(rig.jobs().await.len(), 1, "skip drops the missed firings");
    rig.advance(Duration::from_secs(50 * 60));
    let jobs = rig.wait_for(2).await;
    assert_eq!(jobs.len(), 2);
    assert!(
        keys(&jobs)[1].ends_with("2026-11-12T03:00:00Z"),
        "{:?}",
        keys(&jobs)
    );
}

// ---- failures ----

#[tokio::test]
async fn a_failed_insert_is_planned_again_once_under_the_same_key() {
    let rig = Rig::new().await;
    rig.store.fail_inserts.store(1, Ordering::SeqCst);
    let (stop, task) = rig.start(
        "node-a",
        vec![hosted("acme", "evict", "acme:evict", MissedRunPolicy::Skip)],
    );
    let jobs = rig.wait_for(1).await;
    assert_eq!(jobs.len(), 1, "the next tick enqueued the firing");
    let jobs = rig.settle().await;
    assert_eq!(jobs.len(), 1, "and only once");
    assert_eq!(
        jobs[0].idempotency_key.as_deref(),
        Some("acme:evict:2026-11-12T00:00:00Z")
    );
    assert_eq!(rig.store.fail_inserts.load(Ordering::SeqCst), 0);
    let _ = stop.send(true);
    let _ = task.await;
}

#[tokio::test]
async fn a_coordinator_that_cannot_answer_peek_means_the_node_does_not_lead() {
    let rig = Rig::new().await;
    let schedules = || {
        vec![hosted(
            "acme",
            "evict",
            "acme:evict",
            MissedRunPolicy::RunOnce,
        )]
    };
    // A first node stores the 00:00 mark and stops.
    let (stop_a, task_a) = rig.start("node-a", schedules());
    assert_eq!(rig.wait_for(1).await.len(), 1);
    let _ = stop_a.send(true);
    let _ = task_a.await;

    rig.coordinator.peek_down.store(true, Ordering::SeqCst);
    rig.advance(Duration::from_secs(2 * 3600 + 600));
    let (stop_b, task_b) = rig.start("node-b", schedules());
    let jobs = rig.settle().await;
    assert_eq!(jobs.len(), 1, "no job without the marks: not even `now`");

    rig.coordinator.peek_down.store(false, Ordering::SeqCst);
    let jobs = rig.wait_for(2).await;
    assert_eq!(jobs.len(), 2);
    assert!(
        keys(&jobs)[1].ends_with("2026-11-12T02:00:00Z"),
        "resumed from the stored mark: {:?}",
        keys(&jobs)
    );
    let _ = stop_b.send(true);
    let _ = task_b.await;
}

#[tokio::test]
async fn a_slot_the_queue_refuses_is_reported_and_does_not_stop_the_others() {
    let rig = Rig::new().await;
    let at: Timestamp = "2026-11-12T00:00:00Z".parse().unwrap();
    let mut bad_ref = SlotRef::new("acme", "bad");
    bad_ref.queue = "no_such_queue".to_owned();
    let bad = RunSlot::new("acme", "bad", at, spot_scheduler::SlotOrigin::Due, bad_ref);
    let good = RunSlot::new(
        "acme",
        "good",
        at,
        spot_scheduler::SlotOrigin::Due,
        SlotRef::new("acme", "good"),
    );
    let store: &dyn JobStore = rig.store.as_ref();
    assert!(matches!(
        enqueue_slot(store, &rig.site, &bad).await,
        SlotOutcome::Refused(_)
    ));
    assert_eq!(
        enqueue_slot(store, &rig.site, &good).await,
        SlotOutcome::Enqueued
    );
    assert_eq!(
        enqueue_slot(store, &rig.site, &good).await,
        SlotOutcome::Duplicate,
        "a second insert of the same key is done, not an error"
    );
    assert_eq!(rig.jobs().await.len(), 1);
}

#[tokio::test]
async fn the_metrics_move_on_a_fired_schedule() {
    // The recorder is process-wide; when another test of this binary installed it first, the
    // render below still carries this test's series.
    let installed = spot_telemetry::metrics::install().is_ok();
    let rig = Rig::new().await;
    let (stop, task) = rig.start(
        "node-a",
        vec![hosted(
            "metered",
            "evict",
            "metered:evict",
            MissedRunPolicy::Skip,
        )],
    );
    assert_eq!(rig.wait_for(1).await.len(), 1);
    let _ = stop.send(true);
    let _ = task.await;
    if installed {
        let text = spot_telemetry::metrics::render_prometheus();
        assert!(text.contains("spot_schedule_fired_total"), "{text}");
        assert!(text.contains("spot_schedule_last_fire_seconds"), "{text}");
        assert!(text.contains("metered"), "{text}");
    }
}
