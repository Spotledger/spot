//! Process insight reports (WP-BPM-11.T4): the three insight reports served by the report engine
//! through the report-source port, and the fold scheduled through the internal-schedule port.
//!
//! The tests drive the site's `SourceBook` (what the report service builds from the registry at the
//! start-up check) over a fake `ProcessInsightPort` that holds an insight folded from known
//! timings, so every number is exact and no database is needed: the report compiler, the roles,
//! the runner's permission and field-guard steps, the visibility hooks and the query evaluator are
//! the real ones. The fake decides access and the stuck list by user name, as the real port decides
//! them by permission and initiator (those rules are tested in `process_insight.rs`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_bpm_engine::{CaseId, NodeKind};
use spot_bpm_insight::{
    AllowList, AttemptRef, Insight, NodeRunEvent, ProcessAccess, ProcessKey, ReadAll, ReadNone,
    RunLabels, aggregate,
};
use spot_cli::services::jobs::{InternalScheduleRegistry, InternalSchedules};
use spot_cli::services::process_insight::{
    FoldReport, InsightFuture, InsightPortError, ProcessInsightPort, StuckEntry,
};
use spot_cli::services::process_insight_reports::{
    FOLD_SCHEDULE, SERVICE, ThroughputFeed, WeeklyThroughput, fold_cron, fold_handler, sources,
};
use spot_cli::services::report::{AdmitRequest, ReportSources, SourceBook, SourceRegistry};
use spot_core::{Clock, ManualClock, SiteId};
use spot_dag::NodeId;
use spot_jobs::InMemoryJobStore;
use spot_jobs::model::NewJob;
use spot_jobs::store::{JobFilter, JobStore};
use spot_jobs::worker::{HandlerResult, JobContext, JobHandlerRegistry};
use spot_permissions::Principal;
use spot_scheduler::MissedRunPolicy;
use time::OffsetDateTime;
use time::macros::datetime;

fn t0() -> OffsetDateTime {
    datetime!(2026-11-12 10:00:00 UTC)
}

fn attempt(process: &str, case: &str, node: &str, kind: NodeKind) -> AttemptRef {
    AttemptRef::new(
        ProcessKey::new(process, 1),
        CaseId::new(case).unwrap(),
        NodeId::new(node).unwrap(),
        1,
        kind,
    )
}

/// `orders`: approve takes 10, 20 and 30 s (total 60 s), ship 1 s; `hr`: review 5 s.
fn insight() -> Insight {
    let mut events = Vec::new();
    for (case, seconds) in [("c1", 10), ("c2", 20), ("c3", 30)] {
        let a = attempt("orders", case, "approve", NodeKind::Decision);
        events.push(NodeRunEvent::started(a.clone(), t0()));
        events.push(NodeRunEvent::done(a, t0() + Duration::from_secs(seconds)));
    }
    let ship = attempt("orders", "c1", "ship", NodeKind::Integration);
    events.push(NodeRunEvent::started(ship.clone(), t0()));
    events.push(NodeRunEvent::done(ship, t0() + Duration::from_secs(1)));
    let review = attempt("hr", "h1", "review", NodeKind::Decision);
    events.push(NodeRunEvent::started(review.clone(), t0()));
    events.push(NodeRunEvent::done(review, t0() + Duration::from_secs(5)));
    aggregate(events)
}

fn entry(
    process: &str,
    run: &str,
    node: &str,
    reason: &'static str,
    owner: Option<&str>,
) -> StuckEntry {
    StuckEntry {
        process: process.to_owned(),
        version: 1,
        run: run.to_owned(),
        node: node.to_owned(),
        reason,
        owner: owner.map(str::to_owned),
    }
}

/// The insight port with decisions by user name: `admin` reads everything and supervises,
/// `orders_reader` reads `orders`, everyone else reads nothing. Stuck: `admin` sees all, `starter`
/// is the initiator of r1, `assignee` owns the task of r2, `stranger` sees none.
struct Fake {
    insight: Insight,
    folds: AtomicU32,
    broken: Mutex<bool>,
    fold_error: Mutex<Option<InsightPortError>>,
}

impl Fake {
    fn new(insight: Insight) -> Arc<Fake> {
        Arc::new(Fake {
            insight,
            folds: AtomicU32::new(0),
            broken: Mutex::new(false),
            fold_error: Mutex::new(None),
        })
    }

    fn is_broken(&self) -> bool {
        *self.broken.lock().unwrap()
    }
}

impl ProcessInsightPort for Fake {
    fn fold_once(&self, _now: OffsetDateTime) -> InsightFuture<'_, FoldReport> {
        self.folds.fetch_add(1, Ordering::SeqCst);
        let error = *self.fold_error.lock().unwrap();
        Box::pin(async move {
            match error {
                Some(e) => Err(e),
                None => Ok(FoldReport::default()),
            }
        })
    }

    fn snapshot(&self, _site: &SiteId) -> Result<Insight, InsightPortError> {
        if self.is_broken() {
            return Err(InsightPortError::Store);
        }
        Ok(self.insight.clone())
    }

    fn snapshot_for(&self, site: &SiteId, viewer: &Principal) -> Result<Insight, InsightPortError> {
        let all = self.snapshot(site)?;
        Ok(all.visible_to(&*self.access(viewer)))
    }

    fn access(&self, viewer: &Principal) -> Box<dyn ProcessAccess + Send + Sync> {
        match viewer.user.as_str() {
            "admin" => Box::new(ReadAll),
            "orders_reader" => {
                let mut list = AllowList::new();
                list.insert(ProcessKey::new("orders", 1));
                Box::new(list)
            }
            _ => Box::new(ReadNone),
        }
    }

    fn stuck<'a>(
        &'a self,
        viewer: &'a Principal,
        _now: OffsetDateTime,
    ) -> InsightFuture<'a, Vec<StuckEntry>> {
        let broken = self.is_broken();
        let user = viewer.user.clone();
        Box::pin(async move {
            if broken {
                return Err(InsightPortError::Store);
            }
            let r1 = entry("orders", "r1", "approve", "waiting_for_task", Some("alice"));
            let r2 = entry(
                "orders",
                "r2",
                "approve",
                "waiting_for_task",
                Some("assignee"),
            );
            let r3 = entry("hr", "r3", "review", "timer", None);
            Ok(match user.as_str() {
                "admin" => vec![r1, r2, r3],
                "starter" => vec![r1],
                "assignee" => vec![r2],
                _ => Vec::new(),
            })
        })
    }
}

struct Weeks;

impl ThroughputFeed for Weeks {
    fn weeks(&self, _site: &SiteId) -> Result<Vec<WeeklyThroughput>, ()> {
        Ok(vec![
            WeeklyThroughput {
                process: "orders".to_owned(),
                version: 1,
                week: "2026-W45".to_owned(),
                completed: 4,
            },
            WeeklyThroughput {
                process: "orders".to_owned(),
                version: 1,
                week: "2026-W46".to_owned(),
                completed: 7,
            },
            WeeklyThroughput {
                process: "hr".to_owned(),
                version: 1,
                week: "2026-W46".to_owned(),
                completed: 2,
            },
        ])
    }
}

fn site_clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(t0()))
}

fn book(port: &Arc<Fake>, feed: Option<Arc<dyn ThroughputFeed>>) -> SourceBook {
    let registry = SourceRegistry::new();
    let site = SiteId::new("acme").unwrap();
    let port: Arc<dyn ProcessInsightPort> = Arc::clone(port) as Arc<dyn ProcessInsightPort>;
    let roles: Vec<String> = vec!["Process Reader".to_owned()];
    for source in sources(
        &port,
        feed.as_ref(),
        &site,
        &site_clock(),
        &RunLabels::defaults(),
        &roles,
    )
    .unwrap()
    {
        registry.register(source).unwrap();
    }
    SourceBook::compile(&registry, site_clock())
}

fn who(user: &str) -> AdmitRequest {
    AdmitRequest {
        site: "acme".to_owned(),
        user: user.to_owned(),
        principal: Principal::with_roles(user, &["Process Reader"]),
    }
}

fn cell(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The values of column `name` of a result, as text.
async fn column(book: &SourceBook, user: &str, report: &str, name: &str) -> Vec<String> {
    let (result, _) = book
        .run(&who(user), report, &json!({}), None)
        .await
        .unwrap();
    let at = result
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|row| cell(&row[at])).collect()
}

fn code_of<T>(answer: Result<T, spot_http::api::service::ApiError>) -> String {
    match answer {
        Ok(_) => "ok".to_owned(),
        Err(e) => e.code.as_str().to_owned(),
    }
}

// ---- the reports ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn known_timings_give_exact_integer_aggregates_and_the_bottleneck_is_the_top_row() {
    let book = book(&Fake::new(insight()), None);
    let nodes = column(&book, "admin", "bpm_cycle_time_by_step", "node").await;
    // Ordered by total cycle time, longest first: approve (60 s), review (5 s), ship (1 s).
    assert_eq!(nodes, ["approve", "review", "ship"]);
    assert_eq!(
        column(&book, "admin", "bpm_cycle_time_by_step", "attempt").await,
        ["3", "1", "1"]
    );
    assert_eq!(
        column(&book, "admin", "bpm_cycle_time_by_step", "work_ms").await,
        ["60000", "5000", "1000"]
    );
    let average = column(&book, "admin", "bpm_cycle_time_by_step", "cycle_ms").await;
    // An average is the decimal pair of its sum and its count (the contract's Values table):
    // approve ran 3 attempts for 60 s in all, so its mean is 20 s.
    assert_eq!(average[0], r#"["60000",3]"#, "{average:?}");
    assert_eq!(
        column(&book, "admin", "bpm_cycle_time_by_step", "node_kind").await,
        ["decision", "decision", "integration"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_report_describes_with_run_labels_and_no_word_case() {
    let book = book(&Fake::new(insight()), None);
    let listed = book.list(&who("admin")).await;
    let names: BTreeSet<_> = listed.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from(["bpm_cycle_time_by_step", "bpm_stuck_queue"])
    );
    for summary in &listed {
        assert_eq!(summary.app, SERVICE);
        assert!(!summary.label.contains("Case"), "{}", summary.label);
    }
    let stuck = listed.iter().find(|r| r.name == "bpm_stuck_queue").unwrap();
    assert_eq!(stuck.label, "Stuck Requests");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_without_read_gets_neither_the_rows_of_a_process_nor_a_zero_row() {
    let with = book(&Fake::new(insight()), None);
    let without = book(&Fake::new(aggregate(Vec::new())), None);

    // Reads `orders` only: the `hr` row is absent.
    let processes = column(&with, "orders_reader", "bpm_cycle_time_by_step", "process").await;
    assert_eq!(processes, ["orders", "orders"]);

    // Reads nothing: exactly what a site without the processes answers.
    let (stranger, _) = with
        .run(&who("nobody"), "bpm_cycle_time_by_step", &json!({}), None)
        .await
        .unwrap();
    let (empty_site, _) = without
        .run(&who("nobody"), "bpm_cycle_time_by_step", &json!({}), None)
        .await
        .unwrap();
    assert!(
        stranger.rows.is_empty(),
        "no zero row for a process they may not read"
    );
    assert_eq!(stranger.rows, empty_site.rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stuck_queue_lists_a_run_only_to_its_initiator_assignee_or_supervisor() {
    let book = book(&Fake::new(insight()), None);
    let runs = |user: &'static str| {
        let book = &book;
        async move { column(book, user, "bpm_stuck_queue", "run").await }
    };
    let mut admin = runs("admin").await;
    admin.sort();
    assert_eq!(
        admin,
        ["r1", "r2", "r3"],
        "a supervisor sees every stuck Run"
    );
    assert_eq!(runs("starter").await, ["r1"], "the initiator");
    assert_eq!(
        runs("assignee").await,
        ["r2"],
        "the assignee of the stuck node"
    );
    assert!(runs("stranger").await.is_empty(), "a stranger sees none");

    // Ids, node, reason code and owner only: the reason is the state, the rest is not served.
    assert_eq!(
        column(&book, "starter", "bpm_stuck_queue", "state").await,
        ["waiting_for_task"]
    );
    assert_eq!(
        column(&book, "starter", "bpm_stuck_queue", "owner").await,
        ["alice"]
    );
    assert_eq!(
        column(&book, "starter", "bpm_stuck_queue", "node").await,
        ["approve"]
    );
    assert_eq!(
        column(&book, "starter", "bpm_stuck_queue", "started_at").await,
        ["null"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_that_fails_denies_every_report() {
    let fake = Fake::new(insight());
    let book = book(&fake, None);
    *fake.broken.lock().unwrap() = true;
    for report in ["bpm_cycle_time_by_step", "bpm_stuck_queue"] {
        assert_eq!(
            code_of(book.run(&who("admin"), report, &json!({}), None).await),
            "report.not_found",
            "{report}"
        );
    }
    assert!(
        book.list(&who("admin")).await.is_empty(),
        "denied reports are not listed"
    );
    *fake.broken.lock().unwrap() = false;
    assert_eq!(book.list(&who("admin")).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_value_on_the_wire_is_a_float() {
    let book = book(&Fake::new(insight()), Some(Arc::new(Weeks)));
    for report in [
        "bpm_cycle_time_by_step",
        "bpm_stuck_queue",
        "bpm_throughput_by_week",
    ] {
        let (result, _) = book
            .run(&who("admin"), report, &json!({}), None)
            .await
            .unwrap();
        for row in &result.rows {
            for value in row {
                if let Json::Number(n) = value {
                    assert!(n.is_i64() || n.is_u64(), "{report}: {n}");
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn throughput_is_served_only_when_a_feed_exists_and_only_for_readable_processes() {
    let no_feed = book(&Fake::new(insight()), None);
    assert_eq!(
        code_of(
            no_feed
                .run(&who("admin"), "bpm_throughput_by_week", &json!({}), None)
                .await
        ),
        "report.not_found",
        "not registered without a feed, rather than served empty"
    );

    let book = book(&Fake::new(insight()), Some(Arc::new(Weeks)));
    // Newest week first (the definition orders by week descending); a week of `orders` has its
    // runs summed over versions.
    let weeks = column(&book, "admin", "bpm_throughput_by_week", "week").await;
    assert_eq!(weeks[0], "2026-W46");
    assert_eq!(weeks.len(), 3);
    let reader = column(&book, "orders_reader", "bpm_throughput_by_week", "process").await;
    assert_eq!(reader, ["orders", "orders"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_of_site_a_is_unreachable_from_site_b() {
    let a = book(&Fake::new(insight()), None);
    let b = book(&Fake::new(aggregate(Vec::new())), None);
    // Each site's book answers from its own port: site B's insight is empty.
    assert_eq!(
        column(&a, "admin", "bpm_cycle_time_by_step", "node")
            .await
            .len(),
        3
    );
    assert!(
        column(&b, "admin", "bpm_cycle_time_by_step", "node")
            .await
            .is_empty()
    );
    // And a book with no insight sources serves no such report at all.
    let none = SourceBook::compile(&SourceRegistry::new(), site_clock());
    assert_eq!(
        code_of(
            none.run(&who("admin"), "bpm_cycle_time_by_step", &json!({}), None)
                .await
        ),
        "report.not_found"
    );
    assert!(none.list(&who("admin")).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_without_a_listed_role_does_not_see_the_reports_exist() {
    let book = book(&Fake::new(insight()), None);
    let outsider = AdmitRequest {
        site: "acme".to_owned(),
        user: "admin".to_owned(),
        principal: Principal::with_roles("admin", &["Clerk"]),
    };
    assert!(book.list(&outsider).await.is_empty());
    assert_eq!(
        code_of(
            book.run(&outsider, "bpm_cycle_time_by_step", &json!({}), None)
                .await
        ),
        "report.not_found"
    );
}

// ---- the schedule ----

#[test]
fn the_fold_interval_becomes_a_cron_expression_the_scheduler_accepts() {
    for secs in [1, 7, 10, 59, 60, 90, 300] {
        let cron = fold_cron(secs);
        assert!(spot_manifest::Cron::parse(&cron).is_ok(), "{secs}: {cron}");
    }
    assert_eq!(fold_cron(10), "*/10 * * * * *");
    assert_eq!(fold_cron(60), "0 */1 * * * *");
    assert_eq!(fold_cron(300), "0 */5 * * * *");
}

async fn fold_job(handlers: &JobHandlerRegistry) -> (Arc<InMemoryJobStore>, JobContext) {
    let store = Arc::new(InMemoryJobStore::new());
    let site = SiteId::new("acme").unwrap();
    store
        .insert_if_new(NewJob::new(
            format!("{SERVICE}:{FOLD_SCHEDULE}"),
            json!({}),
            "short".to_owned(),
            site.clone(),
            t0(),
        ))
        .await
        .unwrap();
    let job = store
        .list(JobFilter {
            site: Some(site),
            ..JobFilter::default()
        })
        .await
        .unwrap()
        .remove(0);
    assert!(handlers.get(&job.name).is_some());
    (store, JobContext::new(job, 1, t0()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fold_runs_through_the_registered_handler_and_a_lost_lease_is_not_a_failure() {
    let fake = Fake::new(insight());
    let registry = InternalScheduleRegistry::new();
    registry
        .register(spot_cli::services::jobs::InternalSchedule {
            service: SERVICE.to_owned(),
            name: FOLD_SCHEDULE.to_owned(),
            cron: fold_cron(10),
            missed_run_policy: MissedRunPolicy::RunOnce,
            queue: "short".to_owned(),
            budget: Duration::from_secs(30),
            handler: fold_handler(Arc::clone(&fake) as Arc<dyn ProcessInsightPort>),
        })
        .unwrap();
    assert_eq!(registry.names(), vec!["process_insight:fold".to_owned()]);
    let mut handlers = JobHandlerRegistry::new();
    let planned = registry.install(&[], &mut handlers).unwrap();
    assert_eq!(planned.len(), 1);
    assert_eq!(
        planned[0].2,
        MissedRunPolicy::RunOnce,
        "missed runs coalesce into one"
    );

    let (_store, ctx) = fold_job(&handlers).await;
    let handler = handlers.get("process_insight:fold").unwrap();
    let result = (**handler)(ctx.clone()).await.unwrap();
    assert!(matches!(result, HandlerResult::Ok));
    assert_eq!(fake.folds.load(Ordering::SeqCst), 1, "one fold per run");

    *fake.fold_error.lock().unwrap() = Some(InsightPortError::LeaseLost);
    let result = (**handler)(ctx.clone()).await.unwrap();
    assert!(matches!(result, HandlerResult::Ok), "another node folds");

    *fake.fold_error.lock().unwrap() = Some(InsightPortError::Store);
    let result = (**handler)(ctx).await.unwrap();
    assert!(
        matches!(&result, HandlerResult::Retry(code) if code == "process_insight.store_failed"),
        "a store failure asks for a retry by its code"
    );
}
