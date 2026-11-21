//! The task inbox hosting module over a real PostgreSQL task store (WP-BPM-05.T8; ADR-49,
//! ADR-45, native API 0.11.0).
//!
//! Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, when it is unset (the convention of `tests/process_documents.rs`). The store is
//! the real `PgTaskStore` behind the real `Bridge`; the parts that need a live site (who a
//! credential names, the directory, the document facts, the permission engine, the process
//! engine) are the seams of [`Wiring`], filled with in-memory fakes. Each test owns its runtime
//! and drops its host on the test thread, because the bridge refuses runtime threads.
//!
//! What is covered: an approval end to end; a user outside the role getting one byte-identical
//! answer for get, claim and complete; segregation; a stale `If-Match`; delegation and
//! reassignment with their trail; SLA reminders, auto-approve option B and the re-arm after a
//! restart; a permission source that cannot answer denying; two-site isolation; a failing
//! notifier; idempotent `complete`; the notice pump (WP-BPM-05.T13): an `Assigned` notice queued
//! by `open_task` goes out once after a nudge, a sink that never answers is bounded by the send
//! timeout and the next step still works, and with no Decision node nothing is sent. The HTTP routes are WP-BPM-05.T7's and are tested there; the
//! module's own route registration is exercised by the services registry tests.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deadpool_postgres::Pool;
use serde_json::json;
use spot_bpm_tasks::{
    AssigneeSpec, Decision, Directory, FinalAction, MapDocFacts, MapPrincipals, NoApprovers,
    Priority, Recipient, RecordingEngine, Reminder, SlaPolicy, TaskId, TaskOption, TaskSpec,
    WallClockCalendar,
};
use spot_bpm_tasks_pg::PgTaskStore;
use spot_cli::services::process_tasks::{
    BoundedSink, Callers, DecisionSource, NoticePump, TaskDirectory, TaskHost, TaskNotice,
    TaskNoticeSink, Wiring,
};
use spot_core::{Clock, ManualClock};
use spot_http::api::ports::task::{
    CompleteIn, DelegateIn, ReassignIn, TaskListIn, TaskListView, TaskPortError, TaskState,
    TaskView,
};
use spot_http::api::service::{BoxFuture, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_permissions::PermissionEngine;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use time::OffsetDateTime;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::watch;

static NAMES: AtomicUsize = AtomicUsize::new(0);

fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Fakes for the live-site seams.
// ---------------------------------------------------------------------------------------------

/// The credential's token is the user id; an empty token is no credential.
struct TokenIsUser;

impl Callers for TokenIsUser {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<String, TaskPortError>> {
        Box::pin(async move {
            match ctx.credential.as_ref().map(|c| c.token.as_str()) {
                Some(token) if !token.is_empty() => Ok(token.to_owned()),
                _ => Err(TaskPortError::Unauthenticated),
            }
        })
    }
}

/// A fixed directory: roles to members, users to managers, and who is active.
#[derive(Default)]
struct Staff {
    roles: Vec<(&'static str, &'static str)>,
    managers: Vec<(&'static str, &'static str)>,
    inactive: Vec<&'static str>,
}

impl Directory for Staff {
    fn users_in_role(&self, role: &str) -> Vec<String> {
        self.roles
            .iter()
            .filter(|(r, u)| *r == role && !self.inactive.contains(u))
            .map(|(_, u)| (*u).to_owned())
            .collect()
    }
    fn manager_of(&self, user: &str) -> Option<String> {
        self.managers
            .iter()
            .find(|(u, _)| *u == user)
            .map(|(_, m)| (*m).to_owned())
    }
}

impl TaskDirectory for Staff {
    fn is_active(&self, user: &str) -> bool {
        !self.inactive.contains(&user)
    }
    fn reports_of(&self, manager: &str) -> Option<Vec<String>> {
        Some(
            self.managers
                .iter()
                .filter(|(_, m)| *m == manager)
                .map(|(u, _)| (*u).to_owned())
                .collect(),
        )
    }
    fn roles_of(&self, user: &str) -> Option<Vec<String>> {
        Some(
            self.roles
                .iter()
                .filter(|(_, u)| *u == user)
                .map(|(r, _)| (*r).to_owned())
                .collect(),
        )
    }
}

/// A site with no permission engine: every read of a guarded document is denied.
struct NoEngine;

impl DecisionSource for NoEngine {
    fn engine(&self) -> Option<Arc<PermissionEngine>> {
        None
    }
}

/// A notice sink that always fails and counts its calls.
struct BrokenSink(Arc<AtomicUsize>);

impl TaskNoticeSink for BrokenSink {
    fn deliver<'a>(&'a self, _: &'a TaskNotice) -> BoxFuture<'a, Result<(), String>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err("the notice feed is down".to_owned()) })
    }
}

// ---------------------------------------------------------------------------------------------
// The site under test.
// ---------------------------------------------------------------------------------------------

struct Fixture {
    rt: Runtime,
    db: TestDb,
}

/// A scratch database, or `None` (printing `SKIPPED:`) when no server is configured.
fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let Some(db) = rt.block_on(TestDb::create()) else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return None;
    };
    Some(Fixture { rt, db })
}

fn pool_of(mut config: PgConfig) -> Pool {
    config.pool_max_size = 12;
    PgBackend::connect_unchecked(config)
        .expect("backend")
        .pool()
}

/// A store for `tenant` on a fresh bridge, built on a plain thread (the bridge refuses runtime
/// threads) with its tables created.
fn open_store(config: PgConfig, tenant: &str) -> (Arc<Bridge>, Arc<PgTaskStore>) {
    let tenant = tenant.to_owned();
    std::thread::spawn(move || {
        let pool = pool_of(config);
        let bridge = Arc::new(
            Bridge::new(
                BridgeConfig {
                    name: format!("process_tasks_{}", NAMES.fetch_add(1, Ordering::SeqCst)),
                    workers: 2,
                    max_in_flight: 8,
                    queue_capacity: 64,
                    default_deadline: Duration::from_secs(5),
                    max_deadline: Duration::from_secs(20),
                },
                pool.clone(),
            )
            .expect("bridge"),
        );
        let store = Arc::new(PgTaskStore::new(Arc::clone(&bridge), &pool, &tenant).expect("store"));
        store.prepare().expect("prepare");
        (bridge, store)
    })
    .join()
    .expect("store thread")
}

struct Site {
    host: TaskHost,
    engine: Arc<RecordingEngine>,
    clock: Arc<ManualClock>,
    // Kept alive for the life of the host; dropped on the test thread.
    _bridge: Arc<Bridge>,
}

fn staff() -> Staff {
    Staff {
        roles: vec![
            ("approver", "alice"),
            ("approver", "zed"),
            ("approver", "dana"),
        ],
        managers: vec![("alice", "mgr")],
        inactive: vec!["gone"],
    }
}

fn build(rt: &Runtime, db: &TestDb, tenant: &str, sink: Option<Arc<dyn TaskNoticeSink>>) -> Site {
    let (bridge, store) = open_store(db.config(), tenant);
    let engine = Arc::new(RecordingEngine::new());
    let clock = Arc::new(ManualClock::new(start()));
    let host = rt.block_on(async {
        Wiring {
            site: tenant.to_owned(),
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            runtime: Handle::current(),
            store,
            callers: Arc::new(TokenIsUser),
            directory: Arc::new(staff()),
            principals: Arc::new(MapPrincipals::new()),
            facts: Arc::new(MapDocFacts::new()),
            decisions: Arc::new(NoEngine),
            resolver: Arc::new(NoApprovers),
            engine: Arc::clone(&engine) as Arc<dyn spot_bpm_tasks::TaskEngine>,
            sink,
            calendar: Arc::new(WallClockCalendar),
        }
        .build()
    });
    Site {
        host,
        engine,
        clock,
        _bridge: bridge,
    }
}

fn ctx(tenant: &str, user: &str, key: Option<&str>) -> RequestCtx {
    RequestCtx {
        site: spot_app_api::prelude::SiteId::new(tenant.replace('_', "-")).unwrap(),
        credential: if user.is_empty() {
            None
        } else {
            Some(Credential::new(user))
        },
        idempotency_key: key.map(str::to_owned),
        language: None,
        request_id: RequestId::generate(),
        peer: None,
    }
}

fn options() -> Vec<TaskOption> {
    vec![
        TaskOption {
            label: "Approve".to_owned(),
            decision: Decision::Approve,
            branch: "approve".to_owned(),
        },
        TaskOption {
            label: "Reject".to_owned(),
            decision: Decision::Reject,
            branch: "reject".to_owned(),
        },
    ]
}

/// A task for the role `approver`, created by `zed`, on no guarded document.
fn spec(id: &str, case: &str) -> TaskSpec {
    TaskSpec {
        id: TaskId::new(id).unwrap(),
        case: case.to_owned(),
        node: "approve".to_owned(),
        process: "approval".to_owned(),
        title: "Approve the order?".to_owned(),
        doctype: None,
        subject: None,
        created_by: Some("zed".to_owned()),
        distinct_from_creator: false,
        duty: None,
        scope: None,
        assignee_spec: AssigneeSpec::Role("approver".to_owned()),
        assignees: BTreeSet::new(),
        roles: BTreeSet::from(["approver".to_owned()]),
        options: options(),
        form: json!({ "note": "form-payload-marker" }),
        priority: Priority::Normal,
        due: None,
        sla: None,
        created_at: start(),
    }
}

fn open(site: &Site, spec: TaskSpec) {
    site.host.service().open_task(spec).expect("open task");
}

fn run<T>(rt: &Runtime, work: impl std::future::Future<Output = T>) -> T {
    rt.block_on(work)
}

fn mine() -> TaskListIn {
    TaskListIn {
        view: TaskListView::Mine,
        process: None,
        cursor: None,
        limit: 50,
    }
}

fn approve() -> CompleteIn {
    CompleteIn {
        outcome: "approve".to_owned(),
        comment: None,
        values: None,
    }
}

fn get(f: &Fixture, site: &Site, tenant: &str, user: &str, id: &str) -> TaskView {
    let port = site.host.port();
    run(&f.rt, async {
        port.get(&ctx(tenant, user, None), id).await.map(|d| d.task)
    })
    .expect("task get")
}

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_approval_runs_end_to_end_and_complete_is_idempotent() {
    let Some(f) = fixture() else { return };
    let site = build(&f.rt, &f.db, "site_a", None);
    open(&site, spec("t1", "case_one"));
    let port = site.host.port();

    let page = run(&f.rt, async {
        port.list(&ctx("site_a", "alice", None), &mine()).await
    })
    .unwrap();
    assert_eq!(
        page.tasks.len(),
        1,
        "the task is in the role member's inbox"
    );
    assert!(page.tasks[0].document.is_none());
    let none = run(&f.rt, async {
        port.list(&ctx("site_a", "mallory", None), &mine()).await
    })
    .unwrap();
    assert!(
        none.tasks.is_empty(),
        "a user outside the role sees nothing"
    );

    let claimed = run(&f.rt, async {
        port.claim(&ctx("site_a", "alice", None), "t1", 1).await
    })
    .unwrap();
    assert_eq!(claimed.state, TaskState::Claimed);
    assert_eq!(claimed.claimed_by.as_deref(), Some("alice"));

    let done = run(&f.rt, async {
        port.complete(
            &ctx("site_a", "alice", Some("key-1")),
            "t1",
            claimed.version,
            &approve(),
        )
        .await
    })
    .unwrap();
    assert_eq!(done.task.state, TaskState::Completed);
    assert!(!done.replayed);
    assert_eq!(site.engine.count(), 1, "the Run continued exactly once");

    // The same key and request returns the first outcome, and does not resume the Run again.
    let again = run(&f.rt, async {
        port.complete(
            &ctx("site_a", "alice", Some("key-1")),
            "t1",
            claimed.version,
            &approve(),
        )
        .await
    })
    .unwrap();
    assert!(again.replayed);
    assert_eq!(again.task, done.task);
    assert_eq!(site.engine.count(), 1);

    // A different key on a completed task is `task.not_active`.
    let other = run(&f.rt, async {
        port.complete(
            &ctx("site_a", "alice", Some("key-2")),
            "t1",
            done.task.version,
            &approve(),
        )
        .await
    });
    assert_eq!(other.unwrap_err(), TaskPortError::NotActive);
}

#[test]
fn a_user_outside_the_role_gets_one_answer_for_get_claim_and_complete() {
    let Some(f) = fixture() else { return };
    let site = build(&f.rt, &f.db, "site_a", None);
    open(&site, spec("t1", "case_one"));
    let port = site.host.port();
    let who = ctx("site_a", "mallory", None);

    let real = run(&f.rt, async {
        (
            port.get(&who, "t1").await.unwrap_err(),
            port.claim(&who, "t1", 1).await.unwrap_err(),
            port.complete(&who, "t1", 1, &approve()).await.unwrap_err(),
        )
    });
    let unknown = run(&f.rt, async {
        (
            port.get(&who, "no_such_task").await.unwrap_err(),
            port.claim(&who, "no_such_task", 1).await.unwrap_err(),
            port.complete(&who, "no_such_task", 1, &approve())
                .await
                .unwrap_err(),
        )
    });
    assert_eq!(real.0, TaskPortError::NotFound);
    assert_eq!(
        real, unknown,
        "an outsider cannot tell a real task from none"
    );
    assert_eq!(
        get(&f, &site, "site_a", "alice", "t1").state,
        TaskState::Open
    );
}

#[test]
fn a_missing_credential_is_unauthenticated_and_a_failing_permission_source_denies() {
    let Some(f) = fixture() else { return };
    let site = build(&f.rt, &f.db, "site_a", None);
    // A task on a guarded document whose facts and permission engine cannot be read.
    let mut guarded = spec("t1", "case_one");
    guarded.doctype = Some("Sales Order".to_owned());
    guarded.subject = Some("SO-0001".to_owned());
    open(&site, guarded);
    let port = site.host.port();

    let anonymous = run(&f.rt, async {
        port.list(&ctx("site_a", "", None), &mine()).await
    });
    assert_eq!(anonymous.unwrap_err(), TaskPortError::Unauthenticated);

    // `carol` is no assignee; with no engine and no facts she may not even see the task.
    let carol = run(&f.rt, async {
        port.get(&ctx("site_a", "carol", None), "t1").await
    });
    assert_eq!(carol.unwrap_err(), TaskPortError::NotFound);
    let claim = run(&f.rt, async {
        port.claim(&ctx("site_a", "carol", None), "t1", 1).await
    });
    assert_eq!(claim.unwrap_err(), TaskPortError::NotFound);
}

#[test]
fn the_creator_may_not_approve_their_own_document_and_the_task_is_unchanged() {
    let Some(f) = fixture() else { return };
    let site = build(&f.rt, &f.db, "site_a", None);
    let mut task = spec("t1", "case_one");
    task.distinct_from_creator = true; // `zed` created it and is in the role
    open(&site, task);
    let port = site.host.port();

    let refused = run(&f.rt, async {
        port.complete(&ctx("site_a", "zed", None), "t1", 1, &approve())
            .await
    });
    assert_eq!(refused.unwrap_err(), TaskPortError::Segregation);
    assert_eq!(site.engine.count(), 0, "the Run did not continue");
    let after = get(&f, &site, "site_a", "alice", "t1");
    assert_eq!(after.state, TaskState::Open);
    assert_eq!(after.version, 1, "the task is unchanged");

    // Somebody else in the role may decide it.
    let ok = run(&f.rt, async {
        port.complete(&ctx("site_a", "alice", None), "t1", 1, &approve())
            .await
    });
    assert_eq!(ok.unwrap().task.state, TaskState::Completed);
}

#[test]
fn a_stale_if_match_is_refused_with_the_current_version() {
    let Some(f) = fixture() else { return };
    let site = build(&f.rt, &f.db, "site_a", None);
    open(&site, spec("t1", "case_one"));
    let port = site.host.port();
    let stale = run(&f.rt, async {
        port.claim(&ctx("site_a", "alice", None), "t1", 99).await
    });
    assert_eq!(
        stale.unwrap_err(),
        TaskPortError::StaleVersion { current: 1 }
    );
    let claimed = run(&f.rt, async {
        port.claim(&ctx("site_a", "alice", None), "t1", 1).await
    })
    .unwrap();
    // The old version no longer matches.
    let again = run(&f.rt, async {
        port.release(&ctx("site_a", "alice", None), "t1", 1).await
    });
    assert_eq!(
        again.unwrap_err(),
        TaskPortError::StaleVersion {
            current: claimed.version
        }
    );
}

#[test]
fn delegation_and_reassignment_are_audited_and_refuse_bad_targets() {
    let Some(f) = fixture() else { return };
    let site = build(&f.rt, &f.db, "site_a", None);
    let mut task = spec("t1", "case_one");
    task.roles = BTreeSet::new();
    task.assignees = BTreeSet::from(["alice".to_owned()]);
    task.assignee_spec = AssigneeSpec::Any(vec!["alice".to_owned()]);
    open(&site, task);
    let port = site.host.port();

    // Not oneself, not an inactive user: the same answer.
    for target in ["alice", "gone"] {
        let refused = run(&f.rt, async {
            port.delegate(
                &ctx("site_a", "alice", None),
                "t1",
                1,
                &DelegateIn {
                    to: target.to_owned(),
                    until: None,
                    reason: None,
                },
            )
            .await
        });
        assert_eq!(refused.unwrap_err(), TaskPortError::TargetInvalid);
    }

    let delegated = run(&f.rt, async {
        port.delegate(
            &ctx("site_a", "alice", None),
            "t1",
            1,
            &DelegateIn {
                to: "dana".to_owned(),
                until: None,
                reason: Some("on leave".to_owned()),
            },
        )
        .await
    })
    .unwrap();

    // The manager reassigns it.
    let reassigned = run(&f.rt, async {
        port.reassign(
            &ctx("site_a", "mgr", None),
            "t1",
            delegated.version,
            &ReassignIn {
                to: "bob".to_owned(),
                reason: None,
            },
        )
        .await
    })
    .unwrap();
    assert_eq!(reassigned.assignees, vec!["bob".to_owned()]);

    let detail = run(&f.rt, async {
        port.get(&ctx("site_a", "bob", None), "t1").await
    })
    .unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["created", "delegated", "reassigned"]);
    assert_eq!(detail.events[1].actor, "alice");
    assert_eq!(detail.events[2].actor, "mgr");
}

#[test]
fn a_site_cannot_reach_another_sites_tasks_by_id_list_or_timer() {
    let Some(f) = fixture() else { return };
    let a = build(&f.rt, &f.db, "site_a", None);
    let b = build(&f.rt, &f.db, "site_b", None);
    let mut task = spec("t1", "case_one");
    task.sla = Some(SlaPolicy {
        reminders: vec![Reminder {
            after_seconds: 60,
            to: vec![Recipient::Assignees],
        }],
        ..SlaPolicy::default()
    });
    open(&a, task);

    let port_b = b.host.port();
    let by_id = run(&f.rt, async {
        port_b.get(&ctx("site_b", "alice", None), "t1").await
    });
    assert_eq!(by_id.unwrap_err(), TaskPortError::NotFound);
    let by_list = run(&f.rt, async {
        port_b.list(&ctx("site_b", "alice", None), &mine()).await
    })
    .unwrap();
    assert!(by_list.tasks.is_empty());
    let armed_in_b = run(&f.rt, b.host.rearm()).unwrap();
    assert_eq!(armed_in_b, 0, "site B arms no timer for site A's task");
    b.clock.advance(Duration::from_secs(3600));
    let fired_in_b = run(&f.rt, b.host.tick(b.clock.now_utc())).unwrap();
    assert!(fired_in_b.is_empty());
}

#[test]
fn a_failing_notifier_never_fails_the_action_and_is_counted() {
    let Some(f) = fixture() else { return };
    let calls = Arc::new(AtomicUsize::new(0));
    let site = build(
        &f.rt,
        &f.db,
        "site_a",
        Some(Arc::new(BrokenSink(Arc::clone(&calls)))),
    );
    open(&site, spec("t1", "case_one"));
    let port = site.host.port();
    let done = run(&f.rt, async {
        port.complete(&ctx("site_a", "alice", None), "t1", 1, &approve())
            .await
    })
    .unwrap();
    assert_eq!(done.task.state, TaskState::Completed);
    assert!(calls.load(Ordering::SeqCst) >= 1, "the sink was tried");
    assert!(
        site.host.stats().notices_failed >= 1,
        "and the failure counted"
    );
}

#[test]
fn sla_timers_survive_a_restart_and_auto_approve_follows_option_b() {
    let Some(f) = fixture() else { return };
    let site = build(&f.rt, &f.db, "site_a", None);

    // A node that declares `sla_auto_approve` and a deadline.
    let mut declared = spec("t1", "case_one");
    declared.sla = Some(SlaPolicy {
        reminders: vec![Reminder {
            after_seconds: 60,
            to: vec![Recipient::Assignees],
        }],
        final_action: FinalAction::AutoApprove,
        final_after_seconds: Some(600),
        auto_approve_declared: true,
        ..SlaPolicy::default()
    });
    open(&site, declared);

    // A node without the flag may not carry an auto-approve final action at all.
    let mut undeclared = spec("t2", "case_two");
    undeclared.sla = Some(SlaPolicy {
        final_action: FinalAction::AutoApprove,
        final_after_seconds: Some(600),
        auto_approve_declared: false,
        ..SlaPolicy::default()
    });
    assert!(site.host.service().open_task(undeclared).is_err());

    // A restart: a second host over the same store has no timers until it re-arms from the
    // task rows.
    drop(site);
    let site = build(&f.rt, &f.db, "site_a", None);
    assert_eq!(run(&f.rt, site.host.rearm()).unwrap(), 1);

    // The reminder fires; the task stays open.
    site.clock.advance(Duration::from_secs(61));
    let fired = run(&f.rt, site.host.tick(site.clock.now_utc())).unwrap();
    assert_eq!(fired, ["t1".to_owned()]);
    assert_eq!(
        get(&f, &site, "site_a", "alice", "t1").state,
        TaskState::Open
    );
    assert_eq!(site.engine.count(), 0);

    // The final deadline approves it as the system, once, and counts it.
    site.clock.advance(Duration::from_secs(600));
    let fired = run(&f.rt, site.host.tick(site.clock.now_utc())).unwrap();
    assert_eq!(fired, ["t1".to_owned()]);
    let detail = run(&f.rt, async {
        site.host
            .port()
            .get(&ctx("site_a", "alice", None), "t1")
            .await
    })
    .unwrap();
    assert_eq!(detail.task.state, TaskState::Completed);
    let last = detail.events.last().unwrap();
    assert_eq!(last.kind.as_str(), "auto_completed");
    assert_eq!(last.actor, "system:sla");
    assert_eq!(site.engine.count(), 1);
    assert_eq!(site.host.stats().auto_approved, 1);
}

// ---------------------------------------------------------------------------------------------
// The notice pump (WP-BPM-05.T13).
//
// `open_task` is what the Decision node handler calls: it stores the task and only queues the
// `Assigned` notice. `FlushingHandler` nudges the pump when the node returns; the tests nudge it
// by hand, because building the engine's `NodeCtx` needs a live Run.
// ---------------------------------------------------------------------------------------------

/// A sink that records the kind and task of every notice it is given.
#[derive(Clone, Default)]
struct RecordingSink(Arc<Mutex<Vec<(String, String)>>>);

impl RecordingSink {
    fn seen(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

impl TaskNoticeSink for RecordingSink {
    fn deliver<'a>(&'a self, notice: &'a TaskNotice) -> BoxFuture<'a, Result<(), String>> {
        self.0
            .lock()
            .unwrap()
            .push((notice.kind.to_owned(), notice.task.clone()));
        Box::pin(async { Ok(()) })
    }
}

/// A sink that never answers.
struct HungSink(Arc<AtomicUsize>);

impl TaskNoticeSink for HungSink {
    fn deliver<'a>(&'a self, _: &'a TaskNotice) -> BoxFuture<'a, Result<(), String>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::pending())
    }
}

/// A pump with no settle time and a short idle period over the site's task service.
fn pump_of(site: &Site) -> Arc<NoticePump> {
    Arc::new(NoticePump::with_timings(
        site.host.service(),
        Duration::ZERO,
        Duration::from_millis(40),
        Duration::from_secs(5),
    ))
}

/// Polls `done` until it is true or `limit` has passed. Whether it became true.
async fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < end {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    done()
}

#[test]
fn an_assigned_notice_queued_by_open_task_goes_out_once_after_the_step() {
    let Some(f) = fixture() else { return };
    let recorder = RecordingSink::default();
    let sink: Arc<dyn TaskNoticeSink> = Arc::new(BoundedSink::new(
        Arc::new(recorder.clone()),
        Duration::from_secs(5),
    ));
    let site = build(&f.rt, &f.db, "site_a", Some(sink));
    let pump = pump_of(&site);
    let (stop, stopped) = watch::channel(false);
    let serving = {
        let pump = Arc::clone(&pump);
        f.rt.spawn(async move { pump.serve(stopped).await })
    };

    // The node's step: the task is stored and its notice queued, and nothing is sent yet.
    open(&site, spec("t1", "case_one"));
    assert!(
        recorder.seen().is_empty(),
        "opening a task does not send by itself"
    );

    // The step returned: the handler wrapper nudges, and the notice goes out.
    pump.nudge();
    let sent = run(
        &f.rt,
        wait_for(Duration::from_secs(5), || !recorder.seen().is_empty()),
    );
    assert!(sent, "the Assigned notice went out after the step");

    // More nudges and idle flushes find an empty queue: still exactly one send.
    pump.nudge();
    pump.nudge();
    run(&f.rt, async {
        tokio::time::sleep(Duration::from_millis(250)).await
    });
    assert_eq!(
        recorder.seen(),
        vec![("assigned".to_owned(), "t1".to_owned())],
        "exactly one Assigned notice"
    );
    assert_eq!(site.host.stats().notices_sent, 1);
    assert_eq!(site.host.stats().notices_failed, 0);

    stop.send(true).unwrap();
    run(&f.rt, serving).unwrap();
}

#[test]
fn a_notifier_that_never_answers_does_not_stall_the_step_and_the_next_step_works() {
    let Some(f) = fixture() else { return };
    let tried = Arc::new(AtomicUsize::new(0));
    let sink: Arc<dyn TaskNoticeSink> = Arc::new(BoundedSink::new(
        Arc::new(HungSink(Arc::clone(&tried))),
        Duration::from_millis(100),
    ));
    let site = build(&f.rt, &f.db, "site_a", Some(sink));
    let pump = pump_of(&site);

    // The step itself never waits on the sink: opening returns at once, notice queued.
    let started = std::time::Instant::now();
    open(&site, spec("t1", "case_one"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "opening a task does not wait for a send"
    );

    // A flush is bounded by the send timeout, drops the notice and changes no stored state.
    let started = std::time::Instant::now();
    run(&f.rt, pump.flush());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a hung sink held the flush for {:?}",
        started.elapsed()
    );
    assert_eq!(tried.load(Ordering::SeqCst), 1, "the sink was tried once");
    assert_eq!(
        site.host.stats().notices_failed,
        1,
        "the timeout is a failure"
    );
    assert_eq!(
        get(&f, &site, "site_a", "alice", "t1").state,
        TaskState::Open,
        "a lost notice changes no stored state"
    );

    // The next step still works: another task opens, flushes (bounded again) and completes.
    open(&site, spec("t2", "case_two"));
    run(&f.rt, pump.flush());
    assert_eq!(tried.load(Ordering::SeqCst), 2);
    let port = site.host.port();
    let claimed = run(&f.rt, async {
        port.claim(&ctx("site_a", "alice", None), "t2", 1).await
    })
    .unwrap();
    let done = run(&f.rt, async {
        port.complete(
            &ctx("site_a", "alice", Some("key-t2")),
            "t2",
            claimed.version,
            &approve(),
        )
        .await
    })
    .unwrap();
    assert_eq!(done.task.state, TaskState::Completed);
}

#[test]
fn a_step_without_a_decision_node_flushes_nothing_and_costs_no_send() {
    let Some(f) = fixture() else { return };
    let recorder = RecordingSink::default();
    let sink: Arc<dyn TaskNoticeSink> = Arc::new(BoundedSink::new(
        Arc::new(recorder.clone()),
        Duration::from_secs(5),
    ));
    let site = build(&f.rt, &f.db, "site_a", Some(sink));
    let pump = pump_of(&site);
    let (stop, stopped) = watch::channel(false);
    let serving = {
        let pump = Arc::clone(&pump);
        f.rt.spawn(async move { pump.serve(stopped).await })
    };

    // Steps that open no task: nudges (as after any node) and idle periods with an empty queue.
    let _enter = f.rt.enter();
    pump.nudge();
    run(&f.rt, pump.flush());
    run(&f.rt, async {
        tokio::time::sleep(Duration::from_millis(250)).await
    });

    assert!(recorder.seen().is_empty(), "nothing was sent");
    let stats = site.host.stats();
    assert_eq!(
        (
            stats.notices_sent,
            stats.notices_failed,
            stats.notices_dropped
        ),
        (0, 0, 0),
        "no send was attempted or counted"
    );

    stop.send(true).unwrap();
    run(&f.rt, serving).unwrap();
}
