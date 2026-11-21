//! The run directory over the process port (WP-BPM-02.T10): `run_list` answered by
//! `PgRunDirectory` through the module's `RunDirectory`, with the tamper-evident cursor.
//!
//! Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, when it is unset; under `SPOT_REQUIRE_PG=1` a missing server fails (as in
//! `tests/process_api.rs`). The process host, the Case table and the directory are real; who the
//! caller is and who is tied to which task is a small fake `Access`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Map;
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_bpm_definitions::DefinitionStore;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_documents::{CaseFileStore, MemoryCaseFile};
use spot_bpm_engine::HandlerRegistry;
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore, PgRunDirectory};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_api::{
    Access, AuditEntry, Caller, Deps, ProcessApi, RunDirectory, SourceUnavailable, Sourced,
};
use spot_cli::services::process_run_directory::directory;
use spot_core::{Clock, SiteId, SystemClock};
use spot_http::api::ports::process::{
    ProcessPort, ProcessPortError, ProcessResult, Run, RunQuery, RunStart,
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
    jiff::Timestamp::new(at.unix_timestamp(), 0).unwrap()
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

/// A process that waits, so its Runs stay listed as running; startable by anyone.
const WAITER: &str = r#"
name = "waiter"
app = "run_directory_test"
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

/// Identity, supervisors and task ties.
#[derive(Default)]
struct FakeAccess {
    users: Mutex<BTreeMap<String, RecordId>>,
    supervisors: Mutex<BTreeSet<String>>,
    parties: Mutex<BTreeSet<(String, String)>>,
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
        _id: RecordId,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }

    fn assigned_cases<'a>(&'a self, user: &'a str) -> Sourced<'a, Vec<String>> {
        Box::pin(async move {
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
            let key = (user.to_owned(), case.to_owned());
            Ok::<bool, SourceUnavailable>(self.parties.lock().unwrap().contains(&key))
        })
    }
}

struct Site {
    api: ProcessApi,
    access: Arc<FakeAccess>,
    tenant: String,
    pool: deadpool_postgres::Pool,
    /// The pool only the directory uses, so a test can close it alone.
    directory_pool: deadpool_postgres::Pool,
}

/// One site on `db`; `with_directory` decides whether `run_list` has a directory behind it.
async fn site(db: &TestDb, tenant: &str, with_directory: bool) -> Site {
    let pool = backend(db, tenant).pool();
    let store = PgCaseStore::new(pool.clone()).with_tenant(tenant);
    store.prepare().await.unwrap();
    let definitions = Arc::new(PgDefinitionStore::new(pool.clone()).with_tenant(tenant));
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
    definitions
        .publish("waiter", WAITER, None, "tester", None, now())
        .await
        .unwrap();
    host.engine()
        .unwrap()
        .publish(&from_text(WAITER).unwrap())
        .unwrap();
    let directory_pool = backend(db, &format!("{tenant}-directory")).pool();
    let listing: Option<Arc<dyn RunDirectory>> = if with_directory {
        let pg = PgRunDirectory::new(directory_pool.clone()).with_tenant(tenant);
        pg.prepare().await.unwrap();
        Some(directory(tenant, pg))
    } else {
        None
    };
    let access = Arc::new(FakeAccess::default());
    let api = ProcessApi::new(Deps {
        site: site_id,
        host,
        definitions,
        case_file: Arc::new(MemoryCaseFile::new()) as Arc<dyn CaseFileStore>,
        access: Arc::clone(&access) as Arc<dyn Access>,
        directory: listing,
        audit: Arc::new(|_entry: AuditEntry| {}),
        clock,
    });
    Site {
        api,
        access,
        tenant: tenant.to_owned(),
        pool,
        directory_pool,
    }
}

impl Site {
    fn ctx(&self, user: &str) -> RequestCtx {
        RequestCtx {
            site: SiteId::new(&self.tenant).unwrap(),
            credential: Some(Credential::new(self.access.user(user))),
            idempotency_key: Some(format!("k-{}", RecordId::new_v7())),
            language: None,
            request_id: RequestId::new("req-1").unwrap(),
            peer: None,
        }
    }

    async fn start(&self, user: &str) -> Run {
        self.api
            .start_run(
                &self.ctx(user),
                "waiter",
                RunStart {
                    input: Map::new(),
                    business_key: None,
                },
            )
            .await
            .unwrap()
    }

    /// Every page of `user`'s `run_list` of `size`, in order: (ids, number of pages).
    async fn walk(&self, user: &str, mine: bool, size: u32) -> (Vec<String>, usize) {
        let (mut ids, mut pages, mut cursor) = (Vec::new(), 0, None);
        loop {
            let page = self
                .api
                .list_runs(
                    &self.ctx(user),
                    RunQuery {
                        process: None,
                        state: None,
                        mine,
                        cursor: cursor.clone(),
                        limit: size,
                    },
                )
                .await
                .unwrap();
            assert!(page.items.len() <= size as usize);
            pages += 1;
            ids.extend(page.items.into_iter().map(|run| run.id));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return (ids, pages),
            }
        }
    }

    /// The tenant's Case ids in the listing order, optionally of one initiator, from SQL.
    async fn stored_order(&self, initiator: Option<&str>) -> Vec<String> {
        let client = self.pool.get().await.unwrap();
        let initiator = initiator.map(|u| self.access.user(u));
        let rows = client
            .query(
                "SELECT case_id FROM spot_bpm_case WHERE tenant = $1 \
                 AND ($2::text IS NULL OR initiator = $2::text) \
                 ORDER BY started_at DESC, case_id DESC",
                &[&self.tenant, &initiator],
            )
            .await
            .unwrap();
        rows.iter().map(|r| r.get(0)).collect()
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

/// 25 Runs: ann 10, bob 8, cat 7 (interleaved).
async fn populate(s: &Site) -> BTreeMap<&'static str, Vec<String>> {
    let mut runs: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let plan = [("ann", 10), ("bob", 8), ("cat", 7)];
    let mut left: BTreeMap<&str, i32> = plan.iter().copied().collect();
    while left.values().any(|n| *n > 0) {
        for (user, _) in plan {
            if left[user] > 0 {
                let run = s.start(user).await;
                runs.entry(user).or_default().push(run.id);
                *left.get_mut(user).unwrap() -= 1;
            }
        }
    }
    runs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ordinary_user_lists_own_runs_plus_a_task_run_newest_first_across_pages() {
    let db = db_or_skip!();
    let s = site(&db, "dir-ordinary", true).await;
    let runs = populate(&s).await;
    // Ann is an assignee of one of Bob's Runs.
    let bobs = runs["bob"][3].clone();
    s.access.tie("ann", &bobs);

    let (ids, pages) = s.walk("ann", false, 10).await;
    assert_eq!(ids.len(), 11, "ten own Runs and one by task");
    assert_eq!(pages, 2);
    let unique: BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "no duplicate across pages");
    let mut expected_set: BTreeSet<String> = runs["ann"].iter().cloned().collect();
    expected_set.insert(bobs.clone());
    assert_eq!(ids.iter().cloned().collect::<BTreeSet<_>>(), expected_set);
    let order: Vec<String> = s
        .stored_order(None)
        .await
        .into_iter()
        .filter(|id| expected_set.contains(id))
        .collect();
    assert_eq!(ids, order, "newest first, as the table orders them");

    // Bob sees only his own Runs.
    let (bob_ids, _) = s.walk("bob", false, 10).await;
    assert_eq!(bob_ids, s.stored_order(Some("bob")).await);
    assert_eq!(bob_ids.len(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_supervisor_lists_everything_and_mine_narrows() {
    let db = db_or_skip!();
    let s = site(&db, "dir-supervisor", true).await;
    let runs = populate(&s).await;
    s.access.supervise("ann");
    let (all, pages) = s.walk("ann", false, 10).await;
    assert_eq!(all.len(), 25);
    assert_eq!(pages, 3);
    assert_eq!(all, s.stored_order(None).await);
    let (mine, _) = s.walk("ann", true, 10).await;
    assert_eq!(mine.len(), 10);
    assert_eq!(mine, s.stored_order(Some("ann")).await);
    assert_eq!(
        mine.iter().cloned().collect::<BTreeSet<_>>(),
        runs["ann"].iter().cloned().collect::<BTreeSet<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cursor_taken_by_one_user_is_refused_for_another() {
    let db = db_or_skip!();
    let s = site(&db, "dir-cursor", true).await;
    populate(&s).await;
    let query = |cursor: Option<String>, mine: bool| RunQuery {
        process: None,
        state: None,
        mine,
        cursor,
        limit: 5,
    };
    let page = s
        .api
        .list_runs(&s.ctx("ann"), query(None, false))
        .await
        .unwrap();
    let cursor = page
        .next_cursor
        .clone()
        .expect("ann has more than five Runs");
    // Ann can use it; Bob cannot; a changed filter cannot; junk cannot.
    assert!(
        s.api
            .list_runs(&s.ctx("ann"), query(Some(cursor.clone()), false))
            .await
            .is_ok()
    );
    for (who, mine, raw) in [
        ("bob", false, cursor.clone()),
        ("ann", true, cursor.clone()),
        ("ann", false, "junk".to_owned()),
        ("ann", false, format!("{cursor}0")),
    ] {
        let refused = s
            .api
            .list_runs(&s.ctx(who), query(Some(raw), mine))
            .await
            .unwrap_err();
        assert_eq!(refused, ProcessPortError::BadCursor, "{who} mine={mine}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_module_disabled_run_list_is_unavailable_and_the_rest_works() {
    let db = db_or_skip!();
    let s = site(&db, "dir-disabled", false).await;
    let run = s.start("ann").await;
    let listed = s
        .api
        .list_runs(
            &s.ctx("ann"),
            RunQuery {
                process: None,
                state: None,
                mine: false,
                cursor: None,
                limit: 10,
            },
        )
        .await;
    assert_eq!(listed.unwrap_err(), ProcessPortError::Unavailable);
    let got = s.api.get_run(&s.ctx("ann"), &run.id).await.unwrap();
    assert_eq!(got.id, run.id);
    let cancelled = s
        .api
        .cancel_run(&s.ctx("ann"), &run.id, "done")
        .await
        .unwrap();
    assert_eq!(cancelled.process, "waiter");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_database_is_unavailable_not_a_panic() {
    let db = db_or_skip!();
    let s = site(&db, "dir-down", true).await;
    s.start("ann").await;
    // The directory's own pool is closed: its connections are gone.
    s.directory_pool.close();
    let listed = s
        .api
        .list_runs(
            &s.ctx("ann"),
            RunQuery {
                process: None,
                state: None,
                mine: false,
                cursor: None,
                limit: 10,
            },
        )
        .await;
    assert_eq!(listed.unwrap_err(), ProcessPortError::Unavailable);
    // The rest of the site is untouched.
    s.start("ann").await;
}
