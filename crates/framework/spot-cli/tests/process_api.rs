//! The process and run operations of one site (`services/process_api.rs`, WP-BPM-02.T6), over a
//! real PostgreSQL and a real `ProcessHost`.
//!
//! Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, when it is unset (the convention of `tests/process.rs`). The port is called
//! directly, as `spot-http`'s routes call it; the routes themselves are tested in `spot-http`
//! (WP-BPM-02.T5). Identity, rights and task ties are a fake [`Access`] (the production one reads
//! the live site, which needs a whole booted host), so each test controls who the caller is,
//! who supervises, who is tied to a run's task and which documents are readable.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, json};
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, Version};
use spot_bpm_definitions::DefinitionStore;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_documents::{CaseFileStore, Entry, MemoryCaseFile, Operation};
use spot_bpm_engine::{CaseId, HandlerRegistry};
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_api::{
    Access, AuditEntry, Caller, Deps, DirectoryQuery, ProcessApi, RunDirectory, SourceUnavailable,
    Sourced, display_key, engine_key,
};
use spot_core::{Clock, SiteId, SystemClock};
use spot_dag::NodeId;
use spot_http::api::ports::process::{
    ListPage, ProcessPort, ProcessPortError, ProcessResult, Run, RunQuery, RunStart, RunState,
};
use spot_http::api::service::{BoxFuture, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, LeaseStore};

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn to_jiff(at: OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::new(
        at.unix_timestamp(),
        i32::try_from(at.nanosecond()).unwrap_or(0),
    )
    .unwrap()
}

fn backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 4;
    PgBackend::connect_unchecked(cfg).unwrap()
}

#[derive(Default)]
struct NullSubmit;
impl JobSubmit for NullSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

/// A process that waits (so its Run stays running until cancelled), startable by anyone.
const WAITER: &str = r#"
name = "waiter"
app = "process_api_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "pause"
kind = "wait"
wait = "duration"
seconds = 3600

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "pause"

[[edges]]
from = "pause"
to = "done"
"#;

/// A process whose only trigger is a document event: never listed, never startable by hand.
const EVENT_ONLY: &str = r#"
name = "on_event"
app = "process_api_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "document_event"
doctype = "widget"
event = "create"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "done"
"#;

// --- the fake site --------------------------------------------------------------------------

/// Identity (token = user name), supervisors, task ties and readable documents, with switches
/// that make each source fail.
#[derive(Default)]
struct FakeAccess {
    users: Mutex<BTreeMap<String, RecordId>>,
    supervisors: Mutex<BTreeSet<String>>,
    parties: Mutex<BTreeSet<(String, String)>>,
    hidden: Mutex<BTreeSet<RecordId>>,
    tasks_down: AtomicBool,
    identity_down: AtomicBool,
}

impl FakeAccess {
    fn user(&self, name: &str) -> String {
        let id = *self
            .users
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_insert_with(RecordId::new_v7);
        id.to_string()
    }
    fn supervise(&self, name: &str) {
        let id = self.user(name);
        self.supervisors.lock().unwrap().insert(id);
    }
    fn tie(&self, name: &str, case: &str) {
        let id = self.user(name);
        self.parties.lock().unwrap().insert((id, case.to_owned()));
    }
}

impl Access for FakeAccess {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, ProcessResult<Caller>> {
        Box::pin(async move {
            if self.identity_down.load(Ordering::SeqCst) {
                return Err(ProcessPortError::Unavailable);
            }
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .ok_or(ProcessPortError::PermissionDenied)?;
            let known = {
                let users = self.users.lock().unwrap();
                users.values().find(|id| id.to_string() == token).copied()
            };
            let user_id = known.ok_or(ProcessPortError::PermissionDenied)?;
            Ok(Caller {
                user: user_id.to_string(),
                user_id,
                principal: Principal {
                    user: user_id.to_string(),
                    assignments: Vec::new(),
                    is_system: false,
                },
            })
        })
    }

    fn supervises(&self, principal: &Principal) -> bool {
        self.supervisors.lock().unwrap().contains(&principal.user)
    }

    fn may_read<'a>(
        &'a self,
        _c: &'a Caller,
        _doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move { !self.hidden.lock().unwrap().contains(&id) })
    }

    fn assigned_cases<'a>(&'a self, user: &'a str) -> Sourced<'a, Vec<String>> {
        Box::pin(async move {
            if self.tasks_down.load(Ordering::SeqCst) {
                return Err(SourceUnavailable);
            }
            let parties = self.parties.lock().unwrap();
            Ok(parties
                .iter()
                .filter(|(u, _)| u == user)
                .map(|(_, c)| c.clone())
                .collect())
        })
    }

    fn is_party<'a>(&'a self, user: &'a str, case: &'a str) -> Sourced<'a, bool> {
        Box::pin(async move {
            if self.tasks_down.load(Ordering::SeqCst) {
                return Err(SourceUnavailable);
            }
            let key = (user.to_owned(), case.to_owned());
            Ok(self.parties.lock().unwrap().contains(&key))
        })
    }
}

/// A listing that applies the visibility rule over `(case, initiator)` pairs, newest first.
#[derive(Default)]
struct FakeDirectory {
    cases: Mutex<Vec<(String, String)>>,
}

impl RunDirectory for FakeDirectory {
    fn page<'a>(&'a self, q: &'a DirectoryQuery) -> BoxFuture<'a, ProcessResult<ListPage<String>>> {
        Box::pin(async move {
            let cases = self.cases.lock().unwrap();
            let items: Vec<String> = cases
                .iter()
                .rev()
                .filter(|(case, initiator)| {
                    q.sees_all || *initiator == q.viewer || q.also_cases.contains(case)
                })
                .take(q.limit as usize)
                .map(|(case, _)| case.clone())
                .collect();
            Ok(ListPage {
                items,
                next_cursor: None,
            })
        })
    }
}

struct Site {
    api: ProcessApi,
    access: Arc<FakeAccess>,
    directory: Arc<FakeDirectory>,
    case_file: Arc<MemoryCaseFile>,
    audit: Arc<Mutex<Vec<AuditEntry>>>,
    name: String,
}

impl Site {
    fn ctx(&self, user: &str) -> RequestCtx {
        self.access.user(user);
        RequestCtx {
            site: SiteId::new(&self.name).unwrap(),
            credential: Some(Credential::new(self.access.user(user))),
            idempotency_key: None,
            language: None,
            request_id: RequestId::new("req-1").unwrap(),
            peer: None,
        }
    }

    /// Starts the `waiter` process as `user` and records the Run in the fake directory.
    async fn start(&self, user: &str, key: Option<&str>) -> ProcessResult<Run> {
        // A start needs a business key or an idempotency key (the HTTP layer always supplies the
        // latter): without a business key, every call here is a fresh start.
        let mut ctx = self.ctx(user);
        if key.is_none() {
            ctx.idempotency_key = Some(format!("auto-{}", RecordId::new_v7()));
        }
        let run = self
            .api
            .start_run(
                &ctx,
                "waiter",
                RunStart {
                    input: Map::new(),
                    business_key: key.map(str::to_owned),
                },
            )
            .await?;
        let initiator = self.access.user(user);
        let mut cases = self.directory.cases.lock().unwrap();
        if !cases.iter().any(|(c, _)| *c == run.id) {
            cases.push((run.id.clone(), initiator));
        }
        Ok(run)
    }

    fn audited(&self) -> Vec<AuditEntry> {
        self.audit.lock().unwrap().clone()
    }
}

/// One site over its own tenant-scoped tables on `db`, with `waiter` and `on_event` published.
async fn site(db: &TestDb, tenant: &str, directory: bool) -> Site {
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
            to_jiff(now()),
        )
        .unwrap();
    let inner: Arc<dyn spot_bpm_engine::TimerPort> =
        Arc::new(spot_bpm_engine::InMemoryTimerPort::new());
    let timers = FencedTimerPort::new(leases, lease, inner);
    let site_id = SiteId::new(tenant).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let host = ProcessHost::build(
        site_id.clone(),
        store,
        timers,
        HandlerRegistry::logic_default(),
        Arc::clone(&definitions),
        Arc::new(NullSubmit),
        Arc::clone(&clock),
    );
    for (name, text) in [("waiter", WAITER), ("on_event", EVENT_ONLY)] {
        definitions
            .publish(name, text, None, "tester", None, now())
            .await
            .unwrap();
        host.engine()
            .unwrap()
            .publish(&from_text(text).unwrap())
            .unwrap();
    }
    let access = Arc::new(FakeAccess::default());
    let dir = Arc::new(FakeDirectory::default());
    let case_file = Arc::new(MemoryCaseFile::new());
    let audit: Arc<Mutex<Vec<AuditEntry>>> = Arc::default();
    let sink = Arc::clone(&audit);
    let listing: Option<Arc<dyn RunDirectory>> = directory.then(|| Arc::clone(&dir) as _);
    let api = ProcessApi::new(Deps {
        site: site_id,
        host,
        definitions,
        case_file: Arc::clone(&case_file) as Arc<dyn CaseFileStore>,
        access: Arc::clone(&access) as Arc<dyn Access>,
        directory: listing,
        audit: Arc::new(move |entry| sink.lock().unwrap().push(entry)),
        clock,
    });
    Site {
        api,
        access,
        directory: dir,
        case_file,
        audit,
        name: tenant.to_owned(),
    }
}

fn query() -> RunQuery {
    RunQuery {
        process: None,
        state: None,
        mine: false,
        cursor: None,
        limit: 50,
    }
}

macro_rules! db_or_skip {
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

// --- tests ----------------------------------------------------------------------------------

#[test]
fn engine_keys_are_namespaced_and_a_foreign_key_is_not_displayed() {
    let key = engine_key("waiter", "u1", Some("PO-7"), Some("ignored"));
    assert_eq!(key.as_deref(), Some("waiter:u1:bk:PO-7"));
    assert_eq!(
        engine_key("waiter", "u1", None, Some("abc")).as_deref(),
        Some("waiter:u1:ik:abc")
    );
    assert_eq!(engine_key("waiter", "u1", None, None), None);
    let stored = key.unwrap();
    assert_eq!(
        display_key("waiter", Some("u1"), &stored).as_deref(),
        Some("PO-7")
    );
    assert_eq!(display_key("waiter", Some("u2"), &stored), None);
    assert_eq!(display_key("waiter", None, &stored), None);
    // An idempotency-derived key is never shown as a business key.
    assert_eq!(display_key("waiter", Some("u1"), "waiter:u1:ik:abc"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_happy_path_of_every_operation() {
    let db = db_or_skip!();
    let s = site(&db, "api-happy", true).await;

    let page = s.api.list_processes(&s.ctx("ann"), None, 50).await.unwrap();
    let names: Vec<&str> = page.items.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["waiter"],
        "a document-event process is not listed"
    );

    let run = s.start("ann", Some("PO-1")).await.unwrap();
    assert_eq!(run.process, "waiter");
    assert_eq!(run.business_key.as_deref(), Some("PO-1"));
    assert!(matches!(run.state, RunState::Running | RunState::Waiting));

    let got = s.api.get_run(&s.ctx("ann"), &run.id).await.unwrap();
    assert_eq!(got.id, run.id);

    let events = s
        .api
        .run_events(&s.ctx("ann"), &run.id, 0, 100)
        .await
        .unwrap();
    assert_eq!(events.first().map(|e| e.kind.as_str()), Some("run.started"));
    assert!(
        events
            .iter()
            .enumerate()
            .all(|(i, e)| e.seq == i as u64 + 1)
    );
    let later = s
        .api
        .run_events(&s.ctx("ann"), &run.id, 1, 100)
        .await
        .unwrap();
    assert_eq!(later.len(), events.len() - 1, "`after` skips what was seen");

    let listed = s.api.list_runs(&s.ctx("ann"), query()).await.unwrap();
    assert_eq!(listed.items.len(), 1);

    let cancelled = s
        .api
        .cancel_run(&s.ctx("ann"), &run.id, "wrong customer")
        .await
        .unwrap();
    assert_eq!(cancelled.state, RunState::Cancelled);
    let again = s.api.cancel_run(&s.ctx("ann"), &run.id, "twice").await;
    assert_eq!(again.unwrap_err(), ProcessPortError::RunNotActive);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_or_non_manual_process_is_not_found_alike() {
    let db = db_or_skip!();
    let s = site(&db, "api-notfound", true).await;
    for name in ["no_such_process", "on_event"] {
        let start = RunStart {
            input: Map::new(),
            business_key: None,
        };
        let refused = s.api.start_run(&s.ctx("ann"), name, start).await;
        assert_eq!(refused.unwrap_err(), ProcessPortError::NotFound, "{name}");
    }
    let audited = s.audited();
    assert_eq!(
        audited.len(),
        2,
        "a refused start is audited like a success"
    );
    assert!(audited.iter().all(|a| a.operation == "run_start"));
    assert!(audited.iter().all(|a| a.outcome == "process.not_found"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn business_keys_never_collide_across_users() {
    let db = db_or_skip!();
    let s = site(&db, "api-keys", true).await;
    let a = s.start("ann", Some("K-1")).await.unwrap();
    let b = s.start("bob", Some("K-1")).await.unwrap();
    assert_ne!(a.id, b.id, "the same key text is two Runs for two people");
    let a_again = s.start("ann", Some("K-1")).await.unwrap();
    assert_eq!(
        a_again.id, a.id,
        "a repeat by the same initiator is the same Run"
    );
    // Bob cannot reach, or learn of, Ann's Run.
    let seen = s.api.get_run(&s.ctx("bob"), &a.id).await;
    assert_eq!(seen.unwrap_err(), ProcessPortError::RunNotFound);
    let missing = s
        .api
        .get_run(&s.ctx("bob"), "no_such_run_id_0123456789")
        .await;
    assert_eq!(missing.unwrap_err(), ProcessPortError::RunNotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_idempotency_key_starts_one_run() {
    let db = db_or_skip!();
    let s = site(&db, "api-idem", true).await;
    let mut ctx = s.ctx("ann");
    ctx.idempotency_key = Some("start-123".to_owned());
    let start = || RunStart {
        input: Map::new(),
        business_key: None,
    };
    let first = s.api.start_run(&ctx, "waiter", start()).await.unwrap();
    let second = s.api.start_run(&ctx, "waiter", start()).await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(
        first.business_key, None,
        "an idempotency key is not a business key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn visibility_matrix_and_cancel_rights() {
    let db = db_or_skip!();
    let s = site(&db, "api-matrix", true).await;
    let run = s.start("ann", None).await.unwrap();
    s.access.tie("cy", &run.id);
    s.access.supervise("sue");
    s.access.user("eve");

    for (who, sees) in [("ann", true), ("cy", true), ("sue", true), ("eve", false)] {
        let got = s.api.get_run(&s.ctx(who), &run.id).await;
        assert_eq!(got.is_ok(), sees, "get_run as {who}");
        let events = s.api.run_events(&s.ctx(who), &run.id, 0, 10).await;
        assert_eq!(events.is_ok(), sees, "run_events as {who}");
        let listed = s.api.list_runs(&s.ctx(who), query()).await.unwrap();
        assert_eq!(listed.items.len(), usize::from(sees), "list_runs as {who}");
    }
    // A person tied to a task sees the Run but may not cancel it.
    let denied = s.api.cancel_run(&s.ctx("cy"), &run.id, "no").await;
    assert_eq!(denied.unwrap_err(), ProcessPortError::PermissionDenied);
    let stranger = s.api.cancel_run(&s.ctx("eve"), &run.id, "no").await;
    assert_eq!(stranger.unwrap_err(), ProcessPortError::RunNotFound);
    let done = s.api.cancel_run(&s.ctx("sue"), &run.id, "supervisor").await;
    assert_eq!(done.unwrap().state, RunState::Cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_source_denies() {
    let db = db_or_skip!();
    let s = site(&db, "api-failclosed", true).await;
    let run = s.start("ann", None).await.unwrap();
    s.access.tie("cy", &run.id);
    s.access.user("eve");

    // The task store errors: a task party is no longer recognised, the initiator still is.
    s.access.tasks_down.store(true, Ordering::SeqCst);
    let cy = s.api.get_run(&s.ctx("cy"), &run.id).await;
    assert_eq!(cy.unwrap_err(), ProcessPortError::RunNotFound);
    let eve = s.api.get_run(&s.ctx("eve"), &run.id).await;
    assert_eq!(eve.unwrap_err(), ProcessPortError::RunNotFound);
    let cy_list = s.api.list_runs(&s.ctx("cy"), query()).await.unwrap();
    assert!(cy_list.items.is_empty());
    assert!(s.api.get_run(&s.ctx("ann"), &run.id).await.is_ok());

    // The identity source errors: nothing is answered.
    s.access.identity_down.store(true, Ordering::SeqCst);
    assert!(s.api.get_run(&s.ctx("ann"), &run.id).await.is_err());
    assert!(s.api.list_processes(&s.ctx("ann"), None, 10).await.is_err());
    // An unknown credential is a denial, not an allowance.
    s.access.identity_down.store(false, Ordering::SeqCst);
    let mut stranger = s.ctx("ann");
    stranger.credential = Some(Credential::new("not-a-user"));
    let denied = s.api.get_run(&stranger, &run.id).await;
    assert_eq!(denied.unwrap_err(), ProcessPortError::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_list_without_a_directory_is_unavailable() {
    let db = db_or_skip!();
    let s = site(&db, "api-nodir", false).await;
    let listed = s.api.list_runs(&s.ctx("ann"), query()).await;
    assert_eq!(listed.unwrap_err(), ProcessPortError::Unavailable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sites_are_isolated() {
    let db = db_or_skip!();
    let a = site(&db, "api-site-a", true).await;
    let b = site(&db, "api-site-b", true).await;
    let run = a.start("ann", Some("K-1")).await.unwrap();
    // The same user, asked of the other site: no Run, however the call is made.
    let ann = *a.access.users.lock().unwrap().get("ann").unwrap();
    b.access.users.lock().unwrap().insert("ann".to_owned(), ann);
    let ctx = b.ctx("ann");
    let by_id = b.api.get_run(&ctx, &run.id).await;
    assert_eq!(by_id.unwrap_err(), ProcessPortError::RunNotFound);
    let listed = b.api.list_runs(&ctx, query()).await.unwrap();
    assert!(listed.items.is_empty());
    // A request for site B handed to site A's port is refused outright.
    let wrong_site = a.api.get_run(&ctx, &run.id).await;
    assert_eq!(wrong_site.unwrap_err(), ProcessPortError::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn documents_are_filtered_by_the_callers_read_right() {
    let db = db_or_skip!();
    let s = site(&db, "api-docs", true).await;
    let run = s.start("ann", None).await.unwrap();
    s.access.tie("cy", &run.id);
    let (open, hidden) = (RecordId::new_v7(), RecordId::new_v7());
    for document in [open, hidden] {
        s.case_file
            .record(&Entry {
                case: CaseId::new(&run.id).unwrap(),
                node: NodeId::new("pause").unwrap(),
                operation: Operation::Update,
                doctype: Name::new("widget").unwrap(),
                document,
                version: Version(1),
                compensation: None,
                receipt: json!({}),
            })
            .unwrap();
    }
    s.access.hidden.lock().unwrap().insert(hidden);
    let got = s.api.get_run(&s.ctx("cy"), &run.id).await.unwrap();
    let ids: Vec<&str> = got.documents.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, vec![open.to_string().as_str()]);
    // Nothing of a document's fields, the variables or a failure text is in any Run response.
    let shown = format!("{got:?}");
    assert!(!shown.contains(&hidden.to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_is_audited_by_the_length_of_its_reason_only() {
    let db = db_or_skip!();
    let s = site(&db, "api-audit", true).await;
    let run = s.start("ann", None).await.unwrap();
    let reason = "customer called to withdraw";
    s.api
        .cancel_run(&s.ctx("ann"), &run.id, reason)
        .await
        .unwrap();
    let audited = s.audited();
    let cancel = audited
        .iter()
        .find(|a| a.operation == "run_cancel")
        .unwrap();
    assert_eq!(cancel.outcome, "ok");
    assert_eq!(cancel.reason_len, Some(reason.chars().count()));
    assert!(
        !format!("{audited:?}").contains(reason),
        "the text is never audited"
    );
    // A refused cancel is audited too.
    let refused = s.api.cancel_run(&s.ctx("ann"), &run.id, "again").await;
    assert!(refused.is_err());
    let last = s.audited().pop().unwrap();
    assert_eq!(
        (last.operation, last.outcome),
        ("run_cancel", "process.run_not_active")
    );
}
