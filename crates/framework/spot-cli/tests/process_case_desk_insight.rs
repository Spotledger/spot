//! WP-AI-07.T19: the BPM case desk over a **real insight fold**. `process_case_desk.rs` fixes the
//! stuck reasons through a seam; here a case really gets stuck (an integration attempt that asked
//! for a retry an hour away), a real `ProcessInsight` folds the node-run log from the real
//! PostgreSQL store, and the production `InsightStuck` source feeds the desk. Proven:
//!
//! - nothing is stuck before the first fold (the desk reads the insight's last fold, not the
//!   store), and the case is stuck once a fold has seen the open attempt past the stuck age;
//! - `case.unstick` on the desk re-enqueues exactly one step; the T10 staleness rules hold: the
//!   case still reads `stuck` until the next fold, a second unstick is a deduplicated re-enqueue
//!   and not a refusal, and after the next fold the case no longer reads stuck;
//! - a case still inside its node's expected time is `Moving` and cannot be unstuck;
//! - with no insight module (no definition of "stuck") no desk is provided.
//!
//! Time is injected everywhere (fixed instants, a manual clock for the desk); no sleeps. Every
//! test needs PostgreSQL (`SPOT_TEST_PG_URL`) and skips, printing `SKIPPED:`, when it is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{
    CaseStore, HandlerRegistry, InMemoryTimerPort, NodeCtx, NodeHandler, NodeKind, NodeOutcome,
    NodeRunLog, TimerPort,
};
use spot_bpm_insight::StuckReason;
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::SiteCtx;
use spot_cli::services::ports::{
    CoordinatedOutbox, JobSpec, JobSubmit, OutboxLease, PortFuture, StopGate,
};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process::case_desk::{
    Fut, InsightStuck, ProcessCaseDesk, REFUSED_MOVING, REFUSED_PERMISSION, StuckSource,
    Supervisors, provide,
};
use spot_cli::services::process_insight::{
    NodeKinds, PermissionSource, ProcessInsight, ProcessInsightPort, Settings,
};
use spot_cli::services::world_access::cases::CaseDesk;
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId as CoordNode};
use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use spot_dag::NodeId;
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_remedies::CaseLife;
use spot_scheduler::{InMemoryLeaseStore, LeaseStore};
use spot_storage_contract::{Change, ChangeFeed, FeedPosition, OutboxRecord, StorageResult};
use time::OffsetDateTime;
use time::macros::datetime;

/// 2023-11-14T22:13:20Z, the instant the case starts at.
fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn after(seconds: u64) -> OffsetDateTime {
    t0() + Duration::from_secs(seconds)
}

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

fn backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 4;
    PgBackend::connect_unchecked(cfg).unwrap()
}

/// The actor of the desk's `unstick`.
const SUPERVISOR: &str = "agent:supervisor";

/// The stuck age of these tests: an open attempt is stuck after ten seconds.
fn settings() -> Settings {
    Settings {
        stuck_after_secs: 10,
        ..Settings::default()
    }
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

/// A decision-node handler that asks for a retry an hour away on its first attempt and succeeds
/// on the second: the attempt waits as an `integration_retry` until it is due.
struct RetryThenDone;

impl NodeHandler for RetryThenDone {
    fn kind(&self) -> NodeKind {
        NodeKind::Decision
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        if ctx.attempt >= 2 {
            NodeOutcome::done()
        } else {
            NodeOutcome::Retry {
                after: ctx.now + time::Duration::hours(1),
            }
        }
    }
}

/// One process whose only real node is the retrying decision.
const PROCESS: &str = r#"
name = "flaky"
app = "desk_insight_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "call"
kind = "decision"
title = "Call the partner"
assignee = "user:alice"
outcomes = ["approve", "reject"]
# Without this the first retry request is the last attempt (a node does not retry by default).
retry_attempts = 3

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "call"

[[edges]]
from = "call"
to = "done"
"#;

/// One site: its process host (engine over the tenant's Case store), the stand-in job store and
/// the tenant name.
struct Site {
    tenant: String,
    host: Arc<ProcessHost>,
    submit: Arc<DedupSubmit>,
}

async fn site(db: &TestDb, tenant: &str) -> Site {
    let store = PgCaseStore::new(backend(db, tenant).pool()).with_tenant(tenant);
    store.prepare().await.unwrap();
    let definitions =
        Arc::new(PgDefinitionStore::new(backend(db, tenant).pool()).with_tenant(tenant));
    definitions.prepare().await.unwrap();
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let lease = leases
        .acquire(
            &format!("timer:{tenant}"),
            "node-1",
            // Long enough that the timer lease still holds when the test steps the engine
            // hours after the start.
            Duration::from_secs(10 * 24 * 3600),
            jiff::Timestamp::new(1_700_000_000, 0).unwrap(),
        )
        .unwrap();
    let inner: Arc<dyn TimerPort> = Arc::new(InMemoryTimerPort::new());
    let timers = FencedTimerPort::new(leases, lease, inner);
    let mut registry = HandlerRegistry::logic_default();
    registry.register(RetryThenDone);
    let submit = Arc::new(DedupSubmit::default());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let host = ProcessHost::build(
        SiteId::new(tenant).unwrap(),
        store,
        timers,
        registry,
        definitions,
        Arc::clone(&submit) as Arc<dyn JobSubmit>,
        clock,
    );
    host.engine()
        .unwrap()
        .publish(&from_text(PROCESS).unwrap())
        .unwrap();
    Site {
        tenant: tenant.to_owned(),
        host,
        submit,
    }
}

impl Site {
    fn id(&self) -> SiteId {
        SiteId::new(&self.tenant).unwrap()
    }

    /// Starts one case at `t0` and runs it until it waits for its retry.
    async fn start(&self, key: &str) -> String {
        let case = self
            .host
            .start_run("flaky", None, serde_json::json!({}), None, key, t0())
            .await
            .unwrap();
        self.host
            .engine()
            .unwrap()
            .run_until_idle(&case, t0(), 20)
            .await
            .unwrap();
        case.to_string()
    }
}

struct RegistryKinds {
    host: Arc<ProcessHost>,
}

impl NodeKinds for RegistryKinds {
    fn kind_of(&self, process: &str, version: u32, node: &NodeId) -> Option<NodeKind> {
        let definition = self.host.engine().ok()?.processes().get(process, version)?;
        definition
            .version
            .nodes
            .iter()
            .find(|n| n.id.as_str() == node.as_str())
            .map(|n| NodeKind::from_model(&n.kind))
    }
}

#[derive(Clone, Default)]
struct EmptyFeed;

impl ChangeFeed for EmptyFeed {
    async fn read_outbox(&self, _a: FeedPosition, _l: u32) -> StorageResult<Vec<OutboxRecord>> {
        Ok(Vec::new())
    }
    async fn read_changes(&self, _a: FeedPosition, _l: u32) -> StorageResult<Vec<Change>> {
        Ok(Vec::new())
    }
}

async fn coordinator() -> Arc<InProcessCoordinator> {
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-12 10:00:00 UTC)));
    InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        CoordNode::new("n1").unwrap(),
        clock,
    )
    .await
    .unwrap()
}

fn outbox(coordinator: &Arc<InProcessCoordinator>, tenant: &str) -> Arc<dyn OutboxLease> {
    Arc::new(CoordinatedOutbox::new(
        SiteId::new(tenant).unwrap(),
        Arc::clone(coordinator) as Arc<dyn spot_coordinator::Coordinator>,
        EmptyFeed,
        "n1".to_owned(),
        Duration::from_secs(30),
        StopGate::new(),
    ))
}

/// The insight reads no document permission here: the desk filters itself and returns ids only.
struct NoPermissions;

impl PermissionSource for NoPermissions {
    fn may_read(&self, _principal: &Principal, _process: &str) -> Result<bool, ()> {
        Ok(false)
    }
    fn may_supervise(&self, _principal: &Principal) -> Result<bool, ()> {
        Ok(false)
    }
}

/// A real insight for `site`, over its own Case store handle (the log it folds).
async fn insight(
    db: &TestDb,
    site: &Site,
    coord: &Arc<InProcessCoordinator>,
) -> Arc<ProcessInsight> {
    let store =
        Arc::new(PgCaseStore::new(backend(db, &site.tenant).pool()).with_tenant(&site.tenant));
    store.prepare().await.unwrap();
    let kinds = Arc::new(RegistryKinds {
        host: Arc::clone(&site.host),
    });
    Arc::new(ProcessInsight::new(
        site.id(),
        Arc::clone(&store) as Arc<dyn NodeRunLog>,
        store as Arc<dyn CaseStore>,
        kinds,
        outbox(coord, &site.tenant),
        Arc::new(NoPermissions),
        settings(),
    ))
}

/// Only the supervisor holds `process_supervise` (the permission engine's side is covered by the
/// insight and permission suites; the desk's own rule is what is under test here).
struct Supervise;

impl Supervisors for Supervise {
    fn may_supervise<'a>(&'a self, actor: &'a str) -> Fut<'a, bool> {
        Box::pin(async move { actor == SUPERVISOR })
    }
}

/// The desk over `site`'s host with the **production** stuck source over `insight`, judging at
/// `clock`.
fn desk(
    site: &Site,
    insight: &Arc<ProcessInsight>,
    clock: &Arc<ManualClock>,
) -> Arc<ProcessCaseDesk> {
    let stuck: Arc<dyn StuckSource> = Arc::new(InsightStuck::new(
        Arc::clone(insight) as Arc<dyn ProcessInsightPort>,
        site.id(),
        &settings(),
    ));
    Arc::new(ProcessCaseDesk::new(
        Arc::clone(&site.host),
        stuck,
        Arc::new(Supervise),
        Arc::clone(clock) as Arc<dyn Clock>,
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_case_stuck_in_a_real_fold_is_listed_unstuck_once_and_clears_after_the_next_fold() {
    let db = db!();
    let site = site(&db, "desk-ins-a").await;
    let coord = coordinator().await;
    let insight = insight(&db, &site, &coord).await;
    let case = site.start("flaky-1").await;
    let clock = Arc::new(ManualClock::new(after(60)));
    let desk = desk(&site, &insight, &clock);

    // Before the first fold the insight knows nothing: the case is moving, not stuck.
    assert!(desk.stuck().await.unwrap().is_empty());
    assert_eq!(desk.life(&case).await.unwrap(), Some(CaseLife::Running));

    // One real fold of the node-run log: the attempt has waited for its retry since t0.
    let report = insight.fold_once(after(1)).await.unwrap();
    assert!(report.folded > 0, "the fold saw the log: {report:?}");
    let stuck = insight
        .snapshot(&site.id())
        .unwrap()
        .stuck_cases(after(60), &settings().stuck_policy());
    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0].case.to_string(), case);
    assert_eq!(stuck[0].reason, StuckReason::IntegrationRetry);

    // The desk lists it (an integration retry is something the platform can clear).
    assert_eq!(desk.stuck().await.unwrap(), vec![case.clone()]);
    assert_eq!(desk.life(&case).await.unwrap(), Some(CaseLife::Stuck));

    // Unstick re-enqueues exactly one case step.
    site.submit.reset();
    let at = after(60).unix_timestamp();
    assert_eq!(
        desk.unstick(SUPERVISOR, &case, at).await.unwrap(),
        CaseLife::Running
    );
    assert_eq!(site.submit.added(), 1, "exactly one step was queued");

    // Staleness, as the T10 report states it: the insight has not folded again, so the case
    // still reads stuck.
    assert_eq!(desk.life(&case).await.unwrap(), Some(CaseLife::Stuck));
    assert_eq!(desk.stuck().await.unwrap(), vec![case.clone()]);

    // A second unstick is a deduplicated re-enqueue, not a refusal.
    assert_eq!(
        desk.unstick(SUPERVISOR, &case, at).await.unwrap(),
        CaseLife::Running
    );
    assert_eq!(site.submit.added(), 1, "the second unstick added nothing");
    assert_eq!(site.submit.attempts(), 2, "but it did try to enqueue");

    // The queued step runs (here: the engine stepped by hand once the retry is due) and the case
    // finishes. The desk's `life` reads the store, so it says so at once; the stuck list is the
    // insight's last fold and still names the case until the insight folds again.
    let id = spot_bpm_engine::CaseId::new(&case).unwrap();
    site.host
        .engine()
        .unwrap()
        .run_until_idle(&id, after(3601), 20)
        .await
        .unwrap();
    assert_eq!(desk.life(&case).await.unwrap(), Some(CaseLife::Done));
    assert_eq!(
        desk.stuck().await.unwrap(),
        vec![case.clone()],
        "the list is stale until the next fold"
    );

    // After the next fold the case no longer reads stuck.
    insight.fold_once(after(3602)).await.unwrap();
    assert!(desk.stuck().await.unwrap().is_empty());
    assert!(
        insight
            .snapshot(&site.id())
            .unwrap()
            .stuck_cases(after(3602), &settings().stuck_policy())
            .is_empty()
    );
    // And a finished case is refused, as before.
    assert_eq!(
        desk.unstick(SUPERVISOR, &case, after(3602).unix_timestamp())
            .await
            .unwrap_err(),
        spot_cli::services::process::case_desk::REFUSED_DONE
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_case_inside_its_expected_time_is_moving_and_cannot_be_unstuck() {
    let db = db!();
    let site = site(&db, "desk-ins-b").await;
    let coord = coordinator().await;
    let insight = insight(&db, &site, &coord).await;
    let case = site.start("flaky-2").await;
    insight.fold_once(after(1)).await.unwrap();
    // Five seconds in: the open attempt is younger than the stuck age of ten seconds.
    let clock = Arc::new(ManualClock::new(after(5)));
    let desk = desk(&site, &insight, &clock);

    site.submit.reset();
    assert!(desk.stuck().await.unwrap().is_empty());
    assert_eq!(desk.life(&case).await.unwrap(), Some(CaseLife::Running));
    assert_eq!(
        desk.unstick(SUPERVISOR, &case, after(5).unix_timestamp())
            .await
            .unwrap_err(),
        REFUSED_MOVING
    );
    assert_eq!(site.submit.attempts(), 0, "nothing was queued");

    // Ten seconds later (the manual clock moves, the log does not): stuck.
    clock.advance(Duration::from_secs(55));
    assert_eq!(desk.stuck().await.unwrap(), vec![case.clone()]);
    // An actor without the permission is refused, whatever the case.
    assert_eq!(
        desk.unstick("agent:nobody", &case, after(60).unix_timestamp())
            .await
            .unwrap_err(),
        REFUSED_PERMISSION
    );
    assert_eq!(site.submit.attempts(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_insight_module_no_desk_is_provided() {
    let db = db!();
    let site = site(&db, "desk-ins-c").await;
    // A site context with no `process_insight` port: the stuck rule has no source, so the
    // process module's finaliser provides no desk and the world's cases part reads `no cases`.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut ctx = SiteCtx::new(
        "desk-ins-c",
        Duration::from_secs(1),
        clock,
        BTreeMap::new(),
        true,
    );
    assert!(ctx.port::<dyn ProcessInsightPort>().is_none());
    provide(&mut ctx, &site.host).unwrap();
    assert!(
        ctx.port::<dyn CaseDesk>().is_none(),
        "no insight, no stuck definition, no desk"
    );
}
