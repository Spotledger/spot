//! Process insight, part a (WP-BPM-11.T2), against a real PostgreSQL: the feed from the engine's
//! node-run transition log, the durable checkpoint, restart and rebuild equivalence, access,
//! the stuck-Run visibility matrix, two-site isolation and the memory bound.
//!
//! Every database test skips, printing `SKIPPED:`, when `SPOT_TEST_PG_URL` is unset. The engine
//! is driven directly (`ProcessHost::engine`) at fixed instants, so every number is exact.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{
    CaseId, CaseStore, HandlerRegistry, InMemoryTimerPort, NodeCtx, NodeHandler, NodeKind,
    NodeOutcome, NodeRunLog, NodeRunSeq, TimerPort, WaitSpec,
};
use spot_bpm_insight::{Accumulator, AttemptRef, Insight, NodeRunEvent, ProcessAccess, ProcessKey};
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::ports::{
    ConsumerLease, CoordinatedOutbox, JobSpec, JobSubmit, OutboxLease, PortFuture, StopGate,
};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_insight::{
    CONSUMER, InsightPortError, MAX_METRIC_PROCESSES, MetricLabels, NodeKinds, PermissionSource,
    ProcessInsight, ProcessInsightPort, Settings, TaskOwners, event_of,
};
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId as CoordNode};
use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use spot_dag::NodeId;
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, LeaseStore};
use spot_storage_contract::{Change, ChangeFeed, FeedPosition, OutboxRecord, StorageResult};
use time::OffsetDateTime;
use time::macros::datetime;

/// 2023-11-14T22:13:20Z, the instant every run starts at.
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

#[derive(Default)]
struct NoSubmit;

impl JobSubmit for NoSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

/// A decision handler that waits for a person under the key `approve-task`.
struct HumanWait;

impl NodeHandler for HumanWait {
    fn kind(&self) -> NodeKind {
        NodeKind::Decision
    }

    fn run(&self, _ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        NodeOutcome::Wait(WaitSpec::HumanTask {
            key: "approve-task".to_owned(),
            due: None,
        })
    }
}

/// A process `name`: a trigger, one node `id` with the given body, and a terminal join.
fn process_text(name: &str, id: &str, body: &str) -> String {
    format!(
        "name = \"{name}\"\napp = \"insight_test\"\nversion = 1\nstatus = \"published\"\n\n\
         [[nodes]]\nid = \"start\"\nkind = \"trigger\"\ntrigger = \"manual\"\n\n\
         [[nodes]]\nid = \"{id}\"\n{body}\n\n\
         [[nodes]]\nid = \"done\"\nkind = \"logic\"\nlogic = \"join\"\nterminal = true\n\n\
         [[edges]]\nfrom = \"start\"\nto = \"{id}\"\n\n[[edges]]\nfrom = \"{id}\"\nto = \"done\"\n"
    )
}

fn processes() -> Vec<String> {
    vec![
        process_text("three", "mid", "kind = \"logic\"\nlogic = \"branch\""),
        process_text(
            "waiter",
            "pause",
            "kind = \"wait\"\nwait = \"duration\"\nseconds = 30",
        ),
        process_text(
            "approval",
            "approve",
            "kind = \"decision\"\ntitle = \"Approve?\"\nassignee = \"user:alice\"\noutcomes = [\"approve\", \"reject\"]",
        ),
    ]
}

/// One site: its process host (engine over the tenant's Case store) and the tenant name.
struct Site {
    tenant: String,
    host: Arc<ProcessHost>,
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
            Duration::from_secs(3600),
            jiff::Timestamp::new(1_700_000_000, 0).unwrap(),
        )
        .unwrap();
    let inner: Arc<dyn TimerPort> = Arc::new(InMemoryTimerPort::new());
    let timers = FencedTimerPort::new(leases, lease, inner);
    let mut registry = HandlerRegistry::logic_default();
    registry.register(HumanWait);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let host = ProcessHost::build(
        SiteId::new(tenant).unwrap(),
        store,
        timers,
        registry,
        definitions,
        Arc::new(NoSubmit),
        clock,
    );
    let engine = host.engine().unwrap();
    for text in processes() {
        engine.publish(&from_text(&text).unwrap()).unwrap();
    }
    Site {
        tenant: tenant.to_owned(),
        host,
    }
}

impl Site {
    async fn start(&self, process: &str, key: &str, initiator: Option<&str>) -> CaseId {
        let case = self
            .host
            .start_run(
                process,
                None,
                serde_json::json!({}),
                initiator.map(str::to_owned),
                key,
                t0(),
            )
            .await
            .unwrap();
        self.host
            .engine()
            .unwrap()
            .run_until_idle(&case, t0(), 20)
            .await
            .unwrap();
        case
    }

    async fn advance(&self, case: &CaseId, at: OffsetDateTime) {
        self.host
            .engine()
            .unwrap()
            .run_until_idle(case, at, 20)
            .await
            .unwrap();
    }
}

struct RegistryKinds {
    host: Arc<ProcessHost>,
    /// Process names whose definition is "unknown" (to test the skip).
    hidden: BTreeSet<String>,
}

impl NodeKinds for RegistryKinds {
    fn kind_of(&self, process: &str, version: u32, node: &NodeId) -> Option<NodeKind> {
        if self.hidden.contains(process) {
            return None;
        }
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

fn outbox(
    coordinator: &Arc<InProcessCoordinator>,
    tenant: &str,
    node: &str,
) -> Arc<dyn OutboxLease> {
    Arc::new(CoordinatedOutbox::new(
        SiteId::new(tenant).unwrap(),
        Arc::clone(coordinator) as Arc<dyn spot_coordinator::Coordinator>,
        EmptyFeed,
        node.to_owned(),
        Duration::from_secs(30),
        StopGate::new(),
    ))
}

struct Losing {
    inner: Arc<dyn OutboxLease>,
    lose: AtomicBool,
}

impl OutboxLease for Losing {
    fn acquire<'a>(&'a self, consumer: &'a str) -> PortFuture<'a, ConsumerLease> {
        self.inner.acquire(consumer)
    }
    fn renew<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, ConsumerLease> {
        self.inner.renew(lease)
    }
    fn still_held<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, bool> {
        Box::pin(async move {
            if self.lose.load(Ordering::SeqCst) {
                Ok(false)
            } else {
                self.inner.still_held(lease).await
            }
        })
    }
    fn release<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, ()> {
        self.inner.release(lease)
    }
    fn read<'a>(&'a self, after: FeedPosition, limit: u32) -> PortFuture<'a, Vec<OutboxRecord>> {
        self.inner.read(after, limit)
    }
}

#[derive(Default)]
struct Perms {
    read: Mutex<BTreeMap<String, BTreeSet<String>>>,
    supervisors: Mutex<BTreeSet<String>>,
    fail: AtomicBool,
}

impl Perms {
    fn reads(&self, user: &str, processes: &[&str]) {
        self.read.lock().unwrap().insert(
            user.to_owned(),
            processes.iter().map(|p| (*p).to_owned()).collect(),
        );
    }
    fn supervises(&self, user: &str) {
        self.supervisors.lock().unwrap().insert(user.to_owned());
    }
}

impl PermissionSource for Perms {
    fn may_read(&self, principal: &Principal, process: &str) -> Result<bool, ()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(());
        }
        let supervisor = self.supervisors.lock().unwrap().contains(&principal.user);
        let reads = self
            .read
            .lock()
            .unwrap()
            .get(&principal.user)
            .is_some_and(|set| set.contains(process));
        Ok(supervisor || reads)
    }
    fn may_supervise(&self, principal: &Principal) -> Result<bool, ()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(());
        }
        Ok(self.supervisors.lock().unwrap().contains(&principal.user))
    }
}

#[derive(Default)]
struct Owners {
    owners: Mutex<BTreeMap<(String, String), String>>,
    fail: AtomicBool,
}

impl TaskOwners for Owners {
    fn owner_of<'a>(
        &'a self,
        case: &'a str,
        node: &'a str,
    ) -> spot_cli::services::process_insight::PortFut<'a, Result<Option<String>, ()>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err(());
            }
            Ok(self
                .owners
                .lock()
                .unwrap()
                .get(&(case.to_owned(), node.to_owned()))
                .cloned())
        })
    }
}

fn settings() -> Settings {
    Settings {
        stuck_after_secs: 10,
        ..Settings::default()
    }
}

/// An insight for `site`, over its own Case store handle and a given outbox and permissions.
async fn insight(
    db: &TestDb,
    site: &Site,
    outbox: Arc<dyn OutboxLease>,
    perms: Arc<dyn PermissionSource>,
    owners: Option<Arc<dyn TaskOwners>>,
    hidden: &[&str],
) -> ProcessInsight {
    let store =
        Arc::new(PgCaseStore::new(backend(db, &site.tenant).pool()).with_tenant(&site.tenant));
    store.prepare().await.unwrap();
    let kinds = Arc::new(RegistryKinds {
        host: Arc::clone(&site.host),
        hidden: hidden.iter().map(|h| (*h).to_owned()).collect(),
    });
    let mut insight = ProcessInsight::new(
        SiteId::new(&site.tenant).unwrap(),
        Arc::clone(&store) as Arc<dyn NodeRunLog>,
        store as Arc<dyn CaseStore>,
        kinds,
        outbox,
        perms,
        settings(),
    );
    if let Some(owners) = owners {
        insight = insight.with_owners(owners);
    }
    insight
}

/// An insight with the defaults: node `n1`, no permissions granted, no task owners.
async fn quick(db: &TestDb, site: &Site, coord: &Arc<InProcessCoordinator>) -> ProcessInsight {
    let outbox = outbox(coord, &site.tenant, "n1");
    insight(db, site, outbox, Arc::new(Perms::default()), None, &[]).await
}

/// A fresh fold of the site's whole log, the reference every equality below compares with.
async fn fresh_fold(site: &Site) -> Insight {
    let engine = site.host.engine().unwrap();
    let log: &dyn NodeRunLog = engine.store();
    let kinds = RegistryKinds {
        host: Arc::clone(&site.host),
        hidden: BTreeSet::new(),
    };
    let mut acc = Accumulator::new();
    let mut after = NodeRunSeq(0);
    loop {
        let page = log.list_after(after, 1000).await.unwrap();
        let Some(last) = page.last().map(|r| r.seq) else {
            break;
        };
        for record in &page {
            let kind = kinds
                .kind_of(&record.dag_key, record.dag_version, &record.node)
                .unwrap();
            acc.push(event_of(record, kind).unwrap());
        }
        after = last;
    }
    acc.finish()
}

fn principal(user: &str) -> Principal {
    Principal::with_roles(user, &[])
}

fn names(insight: &Insight) -> BTreeSet<String> {
    insight
        .steps()
        .iter()
        .map(|s| s.process.name.clone())
        .chain(
            insight
                .open_attempts()
                .iter()
                .map(|o| o.process.name.clone()),
        )
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fold_equals_a_fresh_fold_of_the_whole_log_with_exact_integer_percentiles() {
    let db = db!();
    let site = site(&db, "ins-fold").await;
    let coord = coordinator().await;
    let insight = quick(&db, &site, &coord).await;
    let mut waiters = Vec::new();
    for i in 0..3 {
        waiters.push(site.start("waiter", &format!("w-{i}"), Some("ada")).await);
    }
    site.start("three", "t-1", None).await;
    for case in &waiters {
        site.advance(case, after(30)).await;
    }

    let report = insight.fold_once(after(31)).await.unwrap();
    assert!(!report.standby);
    assert!(report.folded > 0);
    assert_eq!(report.skipped, 0);

    let snapshot = insight.snapshot(&SiteId::new("ins-fold").unwrap()).unwrap();
    assert_eq!(
        snapshot,
        fresh_fold(&site).await,
        "the fold is the fold of the log"
    );
    let pause = snapshot
        .steps()
        .iter()
        .find(|s| s.process.name == "waiter" && s.node.as_str() == "pause")
        .unwrap();
    assert_eq!(pause.done, 3);
    assert_eq!(
        pause.wait.as_millis(),
        90_000,
        "three attempts of 30 s of waiting"
    );
    // Percentiles are integers: the nearest-rank bucket value, within 3.2% above the sample.
    let p50 = pause.p50.unwrap().as_millis();
    assert!((30_000..=30_960).contains(&p50), "p50 = {p50}");
    assert_eq!(pause.p90, pause.p50);
    assert_eq!(pause.p99, pause.p50);
    assert!(snapshot.open_attempts().is_empty());

    // The checkpoint and the in-memory position agree with the end of the log.
    let engine = site.host.engine().unwrap();
    let last = engine
        .store()
        .list_after(NodeRunSeq(0), 1000)
        .await
        .unwrap()
        .last()
        .map(|r| r.seq)
        .unwrap();
    let cp = engine
        .store()
        .load_checkpoint(CONSUMER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp.seq, last);
    assert_eq!(insight.position().await, last.0);
    assert_eq!(insight.fold_once(after(32)).await.unwrap().folded, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_resumes_from_the_checkpoint_and_a_rebuild_from_zero_gives_the_same_insight() {
    let db = db!();
    let site = site(&db, "ins-restart").await;
    let coord = coordinator().await;
    let first = quick(&db, &site, &coord).await;
    let mut waiters = Vec::new();
    for i in 0..2 {
        waiters.push(site.start("waiter", &format!("w-{i}"), None).await);
    }
    first.fold_once(after(1)).await.unwrap();
    let before = first
        .snapshot(&SiteId::new("ins-restart").unwrap())
        .unwrap();
    assert_eq!(before.open_attempts().len(), 2, "two Runs wait");
    drop(first);

    // The Runs finish while the module is down; a new module resumes from the checkpoint.
    for case in &waiters {
        site.advance(case, after(30)).await;
    }
    let second = quick(&db, &site, &coord).await;
    assert_eq!(second.position().await, 0, "nothing is in memory yet");
    let report = second.fold_once(after(31)).await.unwrap();
    assert!(report.folded > 0, "the new records are folded");
    let resumed = second
        .snapshot(&SiteId::new("ins-restart").unwrap())
        .unwrap();
    assert_eq!(resumed, fresh_fold(&site).await);
    assert!(resumed.open_attempts().is_empty());

    // A rebuild from zero: a second consumer folds the whole log and gets the same Insight.
    let rebuild = quick(&db, &site, &coord)
        .await
        .with_consumer("process_insight_rebuild")
        .unwrap();
    rebuild.fold_once(after(32)).await.unwrap();
    assert_eq!(
        rebuild
            .snapshot(&SiteId::new("ins-restart").unwrap())
            .unwrap(),
        resumed
    );
    // And so does the live consumer after its checkpoint is deleted.
    let client = backend(&db, "ins-restart-del").pool().get().await.unwrap();
    client
        .execute(
            "DELETE FROM spot_bpm_node_run_consumer WHERE tenant = $1 AND consumer = $2",
            &[&"ins-restart", &CONSUMER],
        )
        .await
        .unwrap();
    let third = quick(&db, &site, &coord).await;
    third.fold_once(after(33)).await.unwrap();
    assert_eq!(
        third
            .snapshot(&SiteId::new("ins-restart").unwrap())
            .unwrap(),
        resumed
    );
}

#[test]
fn a_repeated_batch_after_a_lagging_checkpoint_changes_nothing() {
    // A crash between the fold and the save: the batch is read again from the older checkpoint
    // and pushed onto state that already holds it.
    let attempt = |case: &str| {
        AttemptRef::new(
            ProcessKey::new("p", 1),
            CaseId::new(case).unwrap(),
            NodeId::new("n").unwrap(),
            1,
            NodeKind::Wait,
        )
    };
    let mut events = Vec::new();
    for case in ["c1", "c2", "c3"] {
        events.push(NodeRunEvent::started(attempt(case), t0()));
        events.push(NodeRunEvent::done(attempt(case), after(7)));
    }
    let mut once = Accumulator::new();
    once.push_all(events.clone());
    let mut twice = once.clone();
    twice.push_all(events);
    assert_eq!(once.finish(), twice.finish());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_record_of_an_unknown_process_version_is_skipped_and_counted_by_name_only() {
    let db = db!();
    let site = site(&db, "ins-skip").await;
    let coord = coordinator().await;
    let insight = insight(
        &db,
        &site,
        outbox(&coord, "ins-skip", "n1"),
        Arc::new(Perms::default()),
        None,
        &["three"],
    )
    .await;
    site.start("three", "t-1", None).await;
    let waiter = site.start("waiter", "w-1", None).await;
    site.advance(&waiter, after(30)).await;
    let report = insight.fold_once(after(31)).await.unwrap();
    assert!(report.skipped > 0);
    assert_eq!(insight.metrics().skipped, report.skipped);
    let snapshot = insight.snapshot(&SiteId::new("ins-skip").unwrap()).unwrap();
    assert!(!names(&snapshot).contains("three"));
    assert!(names(&snapshot).contains("waiter"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_hides_a_process_the_viewer_may_not_read_and_a_failing_source_denies() {
    let db = db!();
    let site = site(&db, "ins-access").await;
    let coord = coordinator().await;
    let perms = Arc::new(Perms::default());
    perms.reads("reader", &["three"]);
    perms.reads("both", &["three", "waiter"]);
    let insight = insight(
        &db,
        &site,
        outbox(&coord, "ins-access", "n1"),
        perms.clone(),
        None,
        &[],
    )
    .await;
    site.start("three", "t-1", None).await;
    site.start("waiter", "w-1", None).await;
    insight.fold_once(after(1)).await.unwrap();
    let id = SiteId::new("ins-access").unwrap();

    let all = insight.snapshot_for(&id, &principal("both")).unwrap();
    assert_eq!(
        names(&all),
        BTreeSet::from(["three".to_owned(), "waiter".to_owned()])
    );
    let reader = insight.snapshot_for(&id, &principal("reader")).unwrap();
    assert_eq!(names(&reader), BTreeSet::from(["three".to_owned()]));
    assert!(
        reader.steps().iter().all(|s| s.process.name != "waiter"),
        "absent, not a row of zeros"
    );
    assert!(
        reader
            .open_attempts()
            .iter()
            .all(|o| o.process.name != "waiter")
    );

    // A viewer who reads nothing gets exactly what an empty site gives.
    let nobody = insight.snapshot_for(&id, &principal("nobody")).unwrap();
    assert_eq!(nobody, Accumulator::new().finish());
    let access = insight.access(&principal("reader"));
    assert!(access.may_read(&ProcessKey::new("three", 1)));
    assert!(!access.may_read(&ProcessKey::new("waiter", 1)));

    // The source fails: everyone is denied, the supervisor included.
    perms.supervises("boss");
    assert!(
        !insight
            .snapshot_for(&id, &principal("boss"))
            .unwrap()
            .steps()
            .is_empty()
    );
    perms.fail.store(true, Ordering::SeqCst);
    for user in ["both", "reader", "boss"] {
        assert_eq!(
            insight.snapshot_for(&id, &principal(user)).unwrap(),
            Accumulator::new().finish(),
            "{user} is denied when the permission source fails"
        );
        assert!(
            !insight
                .access(&principal(user))
                .may_read(&ProcessKey::new("three", 1))
        );
    }
    assert!(
        insight
            .stuck(&principal("boss"), after(60))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stuck_run_is_listed_only_to_its_initiator_its_assignee_and_a_supervisor() {
    let db = db!();
    let site = site(&db, "ins-stuck").await;
    let coord = coordinator().await;
    let perms = Arc::new(Perms::default());
    perms.supervises("sup");
    let owners = Arc::new(Owners::default());
    let insight = insight(
        &db,
        &site,
        outbox(&coord, "ins-stuck", "n1"),
        perms,
        Some(owners.clone() as Arc<dyn TaskOwners>),
        &[],
    )
    .await;
    let timer_run = site.start("waiter", "w-1", Some("ada")).await;
    let task_run = site.start("approval", "a-1", Some("ida")).await;
    owners.owners.lock().unwrap().insert(
        (task_run.to_string(), "approve".to_owned()),
        "bob".to_owned(),
    );
    insight.fold_once(after(1)).await.unwrap();

    assert!(
        insight
            .stuck(&principal("sup"), after(5))
            .await
            .unwrap()
            .is_empty()
    );

    let now = after(60);
    let seen = |user: &'static str| {
        let insight = &insight;
        async move {
            insight
                .stuck(&principal(user), now)
                .await
                .unwrap()
                .into_iter()
                .map(|e| (e.run, e.node, e.reason, e.owner))
                .collect::<BTreeSet<_>>()
        }
    };
    let timer_entry = (
        timer_run.to_string(),
        "pause".to_owned(),
        "timer",
        None::<String>,
    );
    let task_entry = (
        task_run.to_string(),
        "approve".to_owned(),
        "waiting_for_task",
        Some("bob".to_owned()),
    );
    assert_eq!(
        seen("sup").await,
        BTreeSet::from([timer_entry.clone(), task_entry.clone()])
    );
    assert_eq!(
        seen("ada").await,
        BTreeSet::from([timer_entry]),
        "the initiator of the timer Run"
    );
    assert_eq!(
        seen("ida").await,
        BTreeSet::from([task_entry.clone()]),
        "the initiator of the task Run"
    );
    assert_eq!(
        seen("bob").await,
        BTreeSet::from([task_entry]),
        "the assignee"
    );
    assert!(seen("eve").await.is_empty(), "a stranger sees nothing");

    // The inbox errors: no owner is shown and the assignee sees nothing; the initiator still does.
    owners.fail.store(true, Ordering::SeqCst);
    assert!(seen("bob").await.is_empty());
    let ida = seen("ida").await;
    assert_eq!(ida.len(), 1);
    assert!(ida.iter().all(|(_, _, _, owner)| owner.is_none()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_lease_drops_the_batch_and_degrades_until_the_next_good_fold() {
    let db = db!();
    let site = site(&db, "ins-lease").await;
    let coord = coordinator().await;
    let losing = Arc::new(Losing {
        inner: outbox(&coord, "ins-lease", "n1"),
        lose: AtomicBool::new(true),
    });
    let insight = insight(
        &db,
        &site,
        losing.clone(),
        Arc::new(Perms::default()),
        None,
        &[],
    )
    .await;
    site.start("waiter", "w-1", None).await;

    assert_eq!(
        insight.fold_once(after(1)).await.unwrap_err(),
        InsightPortError::LeaseLost
    );
    assert_eq!(insight.degraded(), Some(InsightPortError::LeaseLost));
    let engine = site.host.engine().unwrap();
    assert!(
        engine
            .store()
            .load_checkpoint(CONSUMER)
            .await
            .unwrap()
            .is_none(),
        "the dropped batch left no checkpoint"
    );
    assert!(
        insight
            .snapshot(&SiteId::new("ins-lease").unwrap())
            .unwrap()
            .is_empty()
    );

    losing.lose.store(false, Ordering::SeqCst);
    assert!(insight.fold_once(after(2)).await.unwrap().folded > 0);
    assert!(insight.degraded().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_does_not_hold_the_lease_adopts_the_leaders_checkpoint() {
    let db = db!();
    let site = site(&db, "ins-standby").await;
    let coord = coordinator().await;
    let leader_outbox = outbox(&coord, "ins-standby", "n1");
    let leader = insight(
        &db,
        &site,
        leader_outbox.clone(),
        Arc::new(Perms::default()),
        None,
        &[],
    )
    .await;
    let follower = insight(
        &db,
        &site,
        outbox(&coord, "ins-standby", "n2"),
        Arc::new(Perms::default()),
        None,
        &[],
    )
    .await;
    site.start("waiter", "w-1", None).await;
    leader.fold_once(after(1)).await.unwrap();

    // The leader holds the lease; the follower reads what it saved.
    let held = leader_outbox.acquire("process_insight").await.unwrap();
    let report = follower.fold_once(after(2)).await.unwrap();
    assert!(report.standby);
    let id = SiteId::new("ins-standby").unwrap();
    assert_eq!(
        follower.snapshot(&id).unwrap(),
        leader.snapshot(&id).unwrap()
    );
    assert!(!follower.snapshot(&id).unwrap().is_empty());
    leader_outbox.release(&held).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sites_on_one_database_never_see_each_others_runs() {
    let db = db!();
    let a = site(&db, "ins-a").await;
    let b = site(&db, "ins-b").await;
    let coord = coordinator().await;
    let perms = Arc::new(Perms::default());
    perms.supervises("sup");
    let ia = insight(
        &db,
        &a,
        outbox(&coord, "ins-a", "n1"),
        perms.clone(),
        None,
        &[],
    )
    .await;
    let ib = insight(
        &db,
        &b,
        outbox(&coord, "ins-b", "n1"),
        perms.clone(),
        None,
        &[],
    )
    .await;
    a.start("waiter", "shared-key", Some("ada")).await;
    ia.fold_once(after(1)).await.unwrap();
    ib.fold_once(after(1)).await.unwrap();

    let id_a = SiteId::new("ins-a").unwrap();
    let id_b = SiteId::new("ins-b").unwrap();
    assert!(!ia.snapshot(&id_a).unwrap().is_empty());
    assert!(ib.snapshot(&id_b).unwrap().is_empty(), "site B has no runs");
    assert_eq!(
        ib.snapshot(&id_a).unwrap_err(),
        InsightPortError::WrongSite,
        "a snapshot names its own site only"
    );
    // Site A's stuck Run is not listed by site B, to anyone, by id or by list.
    assert_eq!(
        ia.stuck(&principal("sup"), after(60)).await.unwrap().len(),
        1
    );
    assert!(
        ib.stuck(&principal("sup"), after(60))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        ib.stuck(&principal("ada"), after(60))
            .await
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_accumulator_stays_bounded_over_many_runs() {
    // 20 000 attempts of one step: the state is the histogram of the step plus the bounded window
    // of recently ended attempts, never a list of samples.
    let mut acc = Accumulator::new();
    for i in 0..20_000_u64 {
        let attempt = AttemptRef::new(
            ProcessKey::new("p", 1),
            CaseId::new(&format!("c{i}")).unwrap(),
            NodeId::new("n").unwrap(),
            1,
            NodeKind::Wait,
        );
        acc.push(NodeRunEvent::started(attempt.clone(), t0()));
        acc.push(NodeRunEvent::done(
            attempt,
            t0() + Duration::from_millis(1 + i % 5000),
        ));
    }
    let state = acc.export_state();
    assert_eq!(state.steps.len(), 1);
    assert!(state.open.is_empty());
    assert!(state.ended.len() <= 256);
    let size = serde_json::to_vec(&state).unwrap().len();
    assert!(size < 64 * 1024, "the checkpoint state is {size} bytes");
    let insight = acc.finish();
    assert_eq!(insight.steps()[0].done, 20_000);
    // Every number of the snapshot is an integer (ADR-12): no float anywhere.
    fn has_float(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Number(n) => n.is_f64(),
            serde_json::Value::Array(items) => items.iter().any(has_float),
            serde_json::Value::Object(map) => map.values().any(has_float),
            _ => false,
        }
    }
    assert!(!has_float(&serde_json::to_value(&insight).unwrap()));
}

#[test]
fn settings_are_bounded_and_the_metric_labels_stay_bounded_too() {
    assert_eq!(Settings::default().validate(), Ok(()));
    let bad = |batch_size, fold_interval_secs, stuck_after_secs| {
        Settings {
            enabled: true,
            batch_size,
            fold_interval_secs,
            stuck_after_secs,
        }
        .validate()
    };
    assert_eq!(bad(0, 10, 60), Err("batch_size"));
    assert_eq!(bad(1001, 10, 60), Err("batch_size"));
    assert_eq!(bad(10, 0, 60), Err("fold_interval_secs"));
    assert_eq!(bad(10, 10, 0), Err("stuck_after_secs"));
    assert!(toml::from_str::<Settings>("enabeld = true").is_err());
    let labels = MetricLabels::default();
    for i in 0..(MAX_METRIC_PROCESSES + 20) {
        let label = labels.label(&format!("process_{i}"));
        assert_eq!(label == "_other", i >= MAX_METRIC_PROCESSES);
    }
    assert_eq!(labels.distinct(), MAX_METRIC_PROCESSES);
    assert_eq!(labels.label("has space"), "_other");
}
