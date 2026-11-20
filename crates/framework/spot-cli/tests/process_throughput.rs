//! Process throughput (WP-BPM-11.T11): the cached weekly completion counts served as the
//! `ThroughputFeed` port, over a real PostgreSQL server (`SPOT_TEST_PG_URL`; the tests skip,
//! printing `SKIPPED:`, without it, and fail under `SPOT_REQUIRE_PG=1`, as `TestDb::create` does).
//!
//! Runs are completed through the real `ProcessHost` of `tests/process.rs`, at instants the test
//! chooses (the Case's end is the instant passed to the engine), the site clock is a manual clock,
//! and the refresh is driven with `ProcessThroughput::refresh_now`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{CaseId, HandlerRegistry, InMemoryTimerPort, TimerPort};
use spot_bpm_insight::{Insight, ProcessAccess, ReadAll, RunLabels, aggregate};
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore, PgThroughput};
use spot_cli::services::Service;
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_insight::{
    FoldReport, InsightFuture, InsightPortError, ProcessInsightPort, StuckEntry,
};
use spot_cli::services::process_insight_reports::{WeeklyThroughput, sources};
use spot_cli::services::process_throughput::{
    PgWeekSource, ProcessThroughput, Settings, ThroughputService, WeekSource,
};
use spot_cli::services::report::{AdmitRequest, ReportSources, SourceBook, SourceRegistry};
use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, Lease, LeaseStore};
use time::OffsetDateTime;
use time::macros::datetime;

/// Thursday 2026-11-12, ISO week 46.
fn today() -> OffsetDateTime {
    datetime!(2026-11-12 10:00:00 UTC)
}

fn backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 4;
    PgBackend::connect_unchecked(cfg).unwrap()
}

fn to_jiff(at: OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::new(
        at.unix_timestamp(),
        i32::try_from(at.nanosecond()).unwrap_or(0),
    )
    .unwrap()
}

/// A submit port that accepts and forgets: these tests drive the engine directly.
struct Forget;

impl JobSubmit for Forget {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

async fn host(db: &TestDb, tenant: &str) -> Arc<ProcessHost> {
    let store = PgCaseStore::new(backend(db, tenant).pool()).with_tenant(tenant);
    store.prepare().await.unwrap();
    let definitions =
        Arc::new(PgDefinitionStore::new(backend(db, tenant).pool()).with_tenant(tenant));
    definitions.prepare().await.unwrap();
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let lease: Lease = leases
        .acquire(
            &format!("timer:{tenant}"),
            "node-1",
            Duration::from_secs(3600),
            to_jiff(today()),
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
        Arc::new(Forget),
        clock,
    )
}

/// Three nodes, every kind in the default handlers: a Run completes without extra wiring.
const THREE_NODE: &str = r#"
name = "three"
app = "process_throughput_test"
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

/// Publishes versions 1 and 2 of `three` into the host's engine.
fn publish_both(host: &ProcessHost) {
    let engine = host.engine().unwrap();
    engine.publish(&from_text(THREE_NODE).unwrap()).unwrap();
    let v2 = THREE_NODE.replace("version = 1", "version = 2");
    engine.publish(&from_text(&v2).unwrap()).unwrap();
}

/// Starts a Run of `version` under `key` and drives it to completion at `at`.
async fn complete(host: &ProcessHost, version: u32, key: &str, at: OffsetDateTime) -> CaseId {
    let case = host
        .start_run("three", Some(version), json!({}), None, key, at)
        .await
        .unwrap();
    let engine = host.engine().unwrap();
    engine.run_until_idle(&case, at, 20).await.unwrap();
    case
}

fn weeks_setting() -> Settings {
    Settings {
        weeks: 4,
        ..Settings::default()
    }
}

/// A throughput cache for `tenant` over its own pool; the pool is returned so a test can close it.
async fn throughput(
    db: &TestDb,
    tenant: &str,
    clock: Arc<dyn Clock>,
) -> (Arc<ProcessThroughput>, deadpool_postgres::Pool) {
    let pool = backend(db, &format!("tp-{tenant}")).pool();
    let reader = PgThroughput::new(pool.clone()).with_tenant(tenant);
    reader.prepare().await.unwrap();
    let source: Arc<dyn WeekSource> = Arc::new(PgWeekSource::new(reader));
    let cache =
        ProcessThroughput::new(SiteId::new(tenant).unwrap(), source, clock, weeks_setting());
    (Arc::new(cache), pool)
}

fn manual() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(today()))
}

fn row(process: &str, version: u32, week: &str, completed: u64) -> WeeklyThroughput {
    WeeklyThroughput {
        process: process.to_owned(),
        version,
        week: week.to_owned(),
        completed,
    }
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_feed_counts_completed_runs_per_week_and_version() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let host = host(&db, "tp-counts").await;
    publish_both(&host);
    // W45 (Mon 2026-11-02 .. Sun 2026-11-08): two of v1. W46: one of v1, two of v2.
    complete(&host, 1, "a1", datetime!(2026-11-03 09:00:00 UTC)).await;
    complete(&host, 1, "a2", datetime!(2026-11-08 23:59:00 UTC)).await;
    complete(&host, 1, "a3", datetime!(2026-11-09 00:00:00 UTC)).await;
    complete(&host, 2, "b1", datetime!(2026-11-10 12:00:00 UTC)).await;
    complete(&host, 2, "b2", datetime!(2026-11-12 08:00:00 UTC)).await;
    // Older than the 4-week window (it starts Monday 2026-10-12): never counted.
    complete(&host, 1, "old", datetime!(2026-09-01 12:00:00 UTC)).await;
    // Started but not completed: never counted.
    host.start_run("three", Some(1), json!({}), None, "open", today())
        .await
        .unwrap();

    let clock = manual();
    let (cache, _pool) = throughput(&db, "tp-counts", Arc::clone(&clock) as Arc<dyn Clock>).await;
    assert!(cache.refresh_now().await);
    assert_eq!(
        cache.feed().weeks(&site("tp-counts")),
        Ok(vec![
            row("three", 1, "2026-W45", 2),
            row("three", 1, "2026-W46", 1),
            row("three", 2, "2026-W46", 2),
        ])
    );

    // The clock moves into the next week: the window moves with it (4 weeks back from Monday
    // 2026-11-16 starts 2026-10-19), and the new Run lands in W47.
    clock.advance(Duration::from_secs(5 * 24 * 3600));
    complete(&host, 2, "c1", datetime!(2026-11-17 09:00:00 UTC)).await;
    assert!(cache.refresh_now().await);
    let weeks = cache.feed().weeks(&site("tp-counts")).unwrap();
    assert!(weeks.contains(&row("three", 2, "2026-W47", 1)), "{weeks:?}");
}

/// The insight port the report needs to be built: it reads everything and has no insight of its
/// own (the throughput report asks only the feed and the port's access decision).
struct ReadsAll;

impl ProcessInsightPort for ReadsAll {
    fn fold_once(&self, _now: OffsetDateTime) -> InsightFuture<'_, FoldReport> {
        Box::pin(async { Ok(FoldReport::default()) })
    }

    fn snapshot(&self, _site: &SiteId) -> Result<Insight, InsightPortError> {
        Ok(aggregate(Vec::new()))
    }

    fn snapshot_for(
        &self,
        site: &SiteId,
        _viewer: &Principal,
    ) -> Result<Insight, InsightPortError> {
        self.snapshot(site)
    }

    fn access(&self, _viewer: &Principal) -> Box<dyn ProcessAccess + Send + Sync> {
        Box::new(ReadAll)
    }

    fn stuck<'a>(
        &'a self,
        _viewer: &'a Principal,
        _now: OffsetDateTime,
    ) -> InsightFuture<'a, Vec<StuckEntry>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn before_the_first_refresh_the_feed_cannot_answer_and_the_report_is_unavailable() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let host = host(&db, "tp-report").await;
    publish_both(&host);
    complete(&host, 1, "r1", datetime!(2026-11-10 09:00:00 UTC)).await;
    complete(&host, 1, "r2", datetime!(2026-11-11 09:00:00 UTC)).await;

    let clock = manual();
    let (cache, _pool) = throughput(&db, "tp-report", Arc::clone(&clock) as Arc<dyn Clock>).await;
    let feed = cache.feed();
    assert_eq!(
        feed.weeks(&site("tp-report")),
        Err(()),
        "no snapshot, no empty success"
    );

    // The report is registered whenever a feed is provided; with no snapshot it answers an
    // error, never an empty table.
    let registry = SourceRegistry::new();
    let port: Arc<dyn ProcessInsightPort> = Arc::new(ReadsAll);
    let roles = vec!["Process Reader".to_owned()];
    let site_clock: Arc<dyn Clock> = clock;
    for source in sources(
        &port,
        Some(&feed),
        &site("tp-report"),
        &site_clock,
        &RunLabels::defaults(),
        &roles,
    )
    .unwrap()
    {
        registry.register(source).unwrap();
    }
    let book = SourceBook::compile(&registry, Arc::clone(&site_clock));
    let who = AdmitRequest {
        site: "tp-report".to_owned(),
        user: "reader".to_owned(),
        principal: Principal::with_roles("reader", &["Process Reader"]),
    };
    let before = book
        .run(&who, "bpm_throughput_by_week", &json!({}), None)
        .await;
    assert!(
        before.is_err(),
        "an unavailable feed must not be served as an empty report"
    );

    // After the first refresh the same report, built over the same feed handle, serves the rows.
    assert!(cache.refresh_now().await);
    let (result, _) = book
        .run(&who, "bpm_throughput_by_week", &json!({}), None)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1, "one process, one version, one week");
    let week = result
        .columns
        .iter()
        .position(|c| c.name == "week")
        .unwrap();
    assert_eq!(result.rows[0][week], json!("2026-W46"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_database_after_a_good_refresh_keeps_serving_the_last_snapshot() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let host = host(&db, "tp-stop").await;
    publish_both(&host);
    complete(&host, 1, "s1", datetime!(2026-11-10 09:00:00 UTC)).await;

    let (cache, pool) = throughput(&db, "tp-stop", manual() as Arc<dyn Clock>).await;
    assert!(cache.refresh_now().await);
    let good = cache.feed().weeks(&site("tp-stop")).unwrap();
    assert_eq!(good, vec![row("three", 1, "2026-W46", 1)]);

    // Closing the reader's pool makes every later read fail, as an unreachable server would.
    pool.close();
    assert!(!cache.refresh_now().await, "the refresh failed");
    assert!(!cache.refresh_now().await, "and again: one failure streak");
    assert_eq!(cache.feed().weeks(&site("tp-stop")), Ok(good));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_sites_data_never_appears() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let a = host(&db, "tp-site-a").await;
    let b = host(&db, "tp-site-b").await;
    publish_both(&a);
    publish_both(&b);
    complete(&a, 1, "a1", datetime!(2026-11-10 09:00:00 UTC)).await;
    complete(&b, 1, "b1", datetime!(2026-11-10 09:00:00 UTC)).await;
    complete(&b, 2, "b2", datetime!(2026-11-11 09:00:00 UTC)).await;

    let (cache_a, _pa) = throughput(&db, "tp-site-a", manual() as Arc<dyn Clock>).await;
    assert!(cache_a.refresh_now().await);
    let feed = cache_a.feed();
    assert_eq!(
        feed.weeks(&site("tp-site-a")),
        Ok(vec![row("three", 1, "2026-W46", 1)])
    );
    // A's cache refuses to speak for B, whatever B has.
    assert_eq!(feed.weeks(&site("tp-site-b")), Err(()));

    let (cache_b, _pb) = throughput(&db, "tp-site-b", manual() as Arc<dyn Clock>).await;
    assert!(cache_b.refresh_now().await);
    assert_eq!(
        cache_b.feed().weeks(&site("tp-site-b")),
        Ok(vec![
            row("three", 1, "2026-W46", 1),
            row("three", 2, "2026-W46", 1)
        ])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_service_refreshes_at_start_and_shutdown_ends_the_loop_within_the_deadline() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let host = host(&db, "tp-loop").await;
    publish_both(&host);
    complete(&host, 1, "l1", datetime!(2026-11-10 09:00:00 UTC)).await;

    let (cache, _pool) = throughput(&db, "tp-loop", manual() as Arc<dyn Clock>).await;
    let service = Arc::new(ThroughputService::new(Arc::clone(&cache)));
    assert!(
        service.check().await.is_err(),
        "unhealthy until the first refresh has been tried"
    );

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let runner = Arc::clone(&service);
    let handle = tokio::spawn(async move { runner.run(stop_rx).await });

    // The loop refreshes once at start; poll the service's own check (bounded), not a fixed sleep.
    let started = tokio::time::timeout(Duration::from_secs(20), async {
        while service.check().await.is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(started.is_ok(), "the first refresh was tried");
    assert_eq!(
        cache.feed().weeks(&site("tp-loop")),
        Ok(vec![row("three", 1, "2026-W46", 1)])
    );

    // The refresh interval is minutes; the stop signal must not wait for it.
    stop_tx.send(true).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert_eq!(ended.unwrap().unwrap(), Ok(()));
}
