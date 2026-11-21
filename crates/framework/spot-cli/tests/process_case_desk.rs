//! WP-AI-07.T10: the BPM service's `CaseDesk` over a real process host. A case stuck on a
//! running or integration-retry attempt is listed and unstuck exactly once, a case that waits for
//! a task or a timer is not listed and unstick refuses it, an actor without `process_supervise`
//! is refused and nothing is queued, a case of another site is invisible, and the world's cases
//! part built from the desk runs `case.unstick` as a rehearsal and for real. The stuck reasons
//! and the permission are in-memory seams (their production sources are the insight port and the
//! permission engine, covered by their own tests); the engine, the store and the job enqueue are
//! real. Every test needs PostgreSQL (`SPOT_TEST_PG_URL`) and skips, printing `SKIPPED:`, when it
//! is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{HandlerRegistry, InMemoryTimerPort, TimerPort};
use spot_bpm_insight::StuckReason;
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process::case_desk::{
    Fut, ProcessCaseDesk, REFUSED_DONE, REFUSED_NO_CASE, REFUSED_PERMISSION, REFUSED_WAITING,
    StuckSource, Supervisors,
};
use spot_cli::services::world_access::cases::{CaseDesk, from_parts as cases_from_parts};
use spot_core::{Clock, SiteId, SystemClock};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_remedies::{CaseLife, CaseRef};
use spot_scheduler::{InMemoryLeaseStore, LeaseStore};
use tokio::runtime::Handle;

/// 2023-11-14T22:13:20Z.
const NOW: i64 = 1_700_000_000;

fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(NOW).unwrap()
}

fn backend(db: &TestDb, size: usize, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = size;
    PgBackend::connect_unchecked(cfg).unwrap()
}

fn to_jiff(at: time::OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::new(
        at.unix_timestamp(),
        i32::try_from(at.nanosecond()).unwrap_or(0),
    )
    .unwrap()
}

/// A job store stand-in with the real store's idempotency: a key already seen is a duplicate
/// (`Ok(false)`), a new one is added (`Ok(true)`).
#[derive(Default)]
struct DedupSubmit {
    seen: Mutex<BTreeSet<String>>,
    attempts: Mutex<usize>,
}

impl DedupSubmit {
    /// Forgets what was submitted so far (the case's first step was queued when it started).
    fn reset(&self) {
        self.seen.lock().unwrap().clear();
        *self.attempts.lock().unwrap() = 0;
    }

    fn added(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn attempts(&self) -> usize {
        *self.attempts.lock().unwrap()
    }
}

impl JobSubmit for DedupSubmit {
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async move {
            *self.attempts.lock().unwrap() += 1;
            Ok(self.seen.lock().unwrap().insert(job.idempotency_key))
        })
    }
}

async fn host(db: &TestDb, tenant: &str, submit: Arc<dyn JobSubmit>) -> Arc<ProcessHost> {
    let store = PgCaseStore::new(backend(db, 4, tenant).pool()).with_tenant(tenant);
    store.prepare().await.unwrap();
    let definitions =
        Arc::new(PgDefinitionStore::new(backend(db, 4, tenant).pool()).with_tenant(tenant));
    definitions.prepare().await.unwrap();
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let lease = leases
        .acquire(
            &format!("timer:{tenant}"),
            "node-1",
            Duration::from_secs(3600),
            to_jiff(now()),
        )
        .unwrap();
    let inner: Arc<dyn TimerPort> = Arc::new(InMemoryTimerPort::new());
    let timers = FencedTimerPort::new(leases, lease, inner);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    ProcessHost::build(
        SiteId::new(tenant).unwrap(),
        store,
        timers,
        HandlerRegistry::logic_default(),
        definitions,
        submit,
        clock,
    )
}

const THREE_NODE: &str = r#"
name = "three"
app = "process_desk_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "mid"
kind = "logic"
logic = "branch"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "mid"

[[edges]]
from = "mid"
to = "done"
"#;

async fn start(host: &ProcessHost, key: &str) -> String {
    let version = from_text(THREE_NODE).unwrap();
    // A second publish of the same version is refused by the registry; ignore that.
    let _ = host.engine().unwrap().publish(&version);
    host.start_run("three", None, serde_json::json!({}), None, key, now())
        .await
        .unwrap()
        .to_string()
}

/// The stuck attempts the insight would report, set by the test.
#[derive(Default)]
struct FakeStuck(Mutex<Vec<(String, StuckReason)>>);

impl FakeStuck {
    fn set(&self, case: &str, reason: StuckReason) {
        self.0.lock().unwrap().push((case.to_owned(), reason));
    }
}

impl StuckSource for FakeStuck {
    fn stuck_at(&self, _now: time::OffsetDateTime) -> Vec<(String, StuckReason)> {
        self.0.lock().unwrap().clone()
    }
}

/// Only the named actors hold `process_supervise`.
struct Supervise(Vec<&'static str>);

impl Supervisors for Supervise {
    fn may_supervise<'a>(&'a self, actor: &'a str) -> Fut<'a, bool> {
        Box::pin(async move { self.0.contains(&actor) })
    }
}

const SUPERVISOR: &str = "agent:supervisor";

fn desk(
    host: &Arc<ProcessHost>,
    stuck: &Arc<FakeStuck>,
    supervisors: Vec<&'static str>,
) -> Arc<ProcessCaseDesk> {
    Arc::new(ProcessCaseDesk::new(
        Arc::clone(host),
        Arc::clone(stuck) as Arc<dyn StuckSource>,
        Arc::new(Supervise(supervisors)),
        Arc::new(SystemClock::new()),
    ))
}

async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(work).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_case_stuck_on_an_exhausted_retry_is_listed_and_unstuck_exactly_once() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let submit = Arc::new(DedupSubmit::default());
    let host = host(&db, "desk-a", submit.clone()).await;
    let case = start(&host, "order-1").await;
    submit.reset();
    let stuck = Arc::new(FakeStuck::default());
    stuck.set(&case, StuckReason::IntegrationRetry);
    let desk = desk(&host, &stuck, vec![SUPERVISOR]);

    assert_eq!(desk.stuck().await.unwrap(), vec![case.clone()]);
    assert_eq!(desk.life(&case).await.unwrap(), Some(CaseLife::Stuck));

    let after = desk.unstick(SUPERVISOR, &case, NOW).await.unwrap();
    assert_eq!(after, CaseLife::Running);
    assert_eq!(submit.added(), 1, "exactly one step was queued");

    // The insight has not folded yet, so the case still reads as stuck: a second unstick goes
    // through the same idempotent enqueue and the job store drops it.
    assert_eq!(
        desk.unstick(SUPERVISOR, &case, NOW).await.unwrap(),
        CaseLife::Running
    );
    assert_eq!(submit.added(), 1, "the second unstick added nothing");
    assert_eq!(submit.attempts(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_case_running_too_long_is_stuck_too() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host = host(&db, "desk-run", Arc::new(DedupSubmit::default())).await;
    let case = start(&host, "order-run").await;
    let stuck = Arc::new(FakeStuck::default());
    stuck.set(&case, StuckReason::Running);
    let desk = desk(&host, &stuck, vec![SUPERVISOR]);
    assert_eq!(desk.stuck().await.unwrap(), vec![case]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_case_waiting_for_a_task_or_a_timer_is_not_listed_and_unstick_refuses_it() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let submit = Arc::new(DedupSubmit::default());
    let host = host(&db, "desk-wait", submit.clone()).await;
    let task_case = start(&host, "wait-task").await;
    let timer_case = start(&host, "wait-timer").await;
    submit.reset();
    let stuck = Arc::new(FakeStuck::default());
    stuck.set(&task_case, StuckReason::WaitingForTask);
    stuck.set(&timer_case, StuckReason::Timer);
    let desk = desk(&host, &stuck, vec![SUPERVISOR]);

    assert!(desk.stuck().await.unwrap().is_empty());
    assert_eq!(
        desk.life(&task_case).await.unwrap(),
        Some(CaseLife::Running)
    );
    assert_eq!(
        desk.life(&timer_case).await.unwrap(),
        Some(CaseLife::Running)
    );
    for case in [&task_case, &timer_case] {
        assert_eq!(
            desk.unstick(SUPERVISOR, case, NOW).await.unwrap_err(),
            REFUSED_WAITING
        );
    }
    assert_eq!(submit.attempts(), 0, "nothing was queued");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_actor_without_the_permission_is_refused_and_nothing_is_queued() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let submit = Arc::new(DedupSubmit::default());
    let host = host(&db, "desk-perm", submit.clone()).await;
    let case = start(&host, "perm-1").await;
    submit.reset();
    let stuck = Arc::new(FakeStuck::default());
    stuck.set(&case, StuckReason::Running);
    let desk = desk(&host, &stuck, vec![SUPERVISOR]);
    for actor in ["agent:nobody", "", "user:someone"] {
        assert_eq!(
            desk.unstick(actor, &case, NOW).await.unwrap_err(),
            REFUSED_PERMISSION
        );
    }
    assert_eq!(submit.attempts(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_case_of_another_site_is_invisible() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host_a = host(&db, "desk-site-a", Arc::new(DedupSubmit::default())).await;
    let submit_b = Arc::new(DedupSubmit::default());
    let host_b = host(&db, "desk-site-b", submit_b.clone()).await;
    let case = start(&host_a, "site-a-case").await;
    // Even a stuck source that names it (the insight of the other site never would) cannot make
    // site B see site A's case: the store is the tenant's own.
    let stuck = Arc::new(FakeStuck::default());
    stuck.set(&case, StuckReason::Running);
    let desk_b = desk(&host_b, &stuck, vec![SUPERVISOR]);
    assert_eq!(desk_b.life(&case).await.unwrap(), None);
    assert_eq!(
        desk_b.unstick(SUPERVISOR, &case, NOW).await.unwrap_err(),
        REFUSED_NO_CASE
    );
    assert_eq!(submit_b.attempts(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_case_is_done_and_cannot_be_unstuck() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let submit = Arc::new(DedupSubmit::default());
    let host = host(&db, "desk-done", submit.clone()).await;
    let case = start(&host, "done-1").await;
    let id = spot_bpm_engine::CaseId::new(&case).unwrap();
    host.engine()
        .unwrap()
        .run_until_idle(&id, now(), 20)
        .await
        .unwrap();
    submit.reset();
    let stuck = Arc::new(FakeStuck::default());
    // A stale stuck entry for a finished case must not make it stuck.
    stuck.set(&case, StuckReason::Running);
    let desk = desk(&host, &stuck, vec![SUPERVISOR]);
    assert_eq!(desk.life(&case).await.unwrap(), Some(CaseLife::Done));
    assert_eq!(
        desk.unstick(SUPERVISOR, &case, NOW).await.unwrap_err(),
        REFUSED_DONE
    );
    assert_eq!(submit.attempts(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_case_or_a_bad_id_is_no_case() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host = host(&db, "desk-none", Arc::new(DedupSubmit::default())).await;
    let desk = desk(&host, &Arc::new(FakeStuck::default()), vec![SUPERVISOR]);
    assert_eq!(desk.life("case_missing").await.unwrap(), None);
    assert_eq!(desk.life("not a case id!").await.unwrap(), None);
    assert_eq!(
        desk.unstick(SUPERVISOR, "case_missing", NOW)
            .await
            .unwrap_err(),
        REFUSED_NO_CASE
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_worlds_cases_part_runs_case_unstick_as_a_rehearsal_and_for_real() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let submit = Arc::new(DedupSubmit::default());
    let host = host(&db, "desk-part", submit.clone()).await;
    let case = start(&host, "part-1").await;
    submit.reset();
    let stuck = Arc::new(FakeStuck::default());
    stuck.set(&case, StuckReason::IntegrationRetry);
    let desk = desk(&host, &stuck, vec![SUPERVISOR]);
    let part = cases_from_parts(Some(desk as Arc<dyn CaseDesk>), Some(Handle::current())).unwrap();
    let case_ref = CaseRef::new(&case);

    // The dry run: the in-memory copy changes, the site's queue does not.
    let (dry, queued_dry) = {
        let part = Arc::clone(&part);
        let case_ref = case_ref.clone();
        let submit = Arc::clone(&submit);
        blocking(move || {
            let mut ops = part.rehearsal().unwrap();
            let change = ops.unstick(&case_ref, NOW).unwrap();
            (change, submit.attempts())
        })
        .await
    };
    assert_eq!(
        (dry.before, dry.after),
        (CaseLife::Stuck, CaseLife::Running)
    );
    assert_eq!(queued_dry, 0, "a rehearsal never reaches the desk");

    // Apply, as the remedy's actor.
    let applied = {
        let part = Arc::clone(&part);
        let case_ref = case_ref.clone();
        blocking(move || {
            let mut ops = part.live_as(SUPERVISOR);
            assert_eq!(ops.stuck(), vec![case_ref.clone()]);
            ops.unstick(&case_ref, NOW).unwrap()
        })
        .await
    };
    assert_eq!(
        (applied.before, applied.after),
        (CaseLife::Stuck, CaseLife::Running)
    );
    assert_eq!(submit.added(), 1);

    // Without the permission the part reports a refusal and queues nothing more.
    let refused = {
        let part = Arc::clone(&part);
        let case_ref = case_ref.clone();
        blocking(move || part.live_as("agent:nobody").unstick(&case_ref, NOW)).await
    };
    assert_eq!(refused.unwrap_err().detail, REFUSED_PERMISSION);
    assert_eq!(submit.added(), 1);
}
