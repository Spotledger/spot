//! The process routes keep their `Idempotency-Key`s in the site's durable store
//! (WP-BPM-02.T11): a restart or a second node cannot start a Run twice.
//!
//! The real `ProcessApi` over a real `ProcessHost`, mounted with `process_routes` (the function
//! the host module uses) over the real axum router, and a `PgIdempotencyStore` built by
//! `durable_idempotency_store`. A "restart" builds a new port, a new store and a new router on the
//! same database. Every test needs PostgreSQL (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, without it; `SPOT_REQUIRE_PG=1` makes a missing server fail (inside `TestDb`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_bpm_definitions::DefinitionStore;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_documents::{CaseFileStore, MemoryCaseFile};
use spot_bpm_engine::HandlerRegistry;
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_api::{
    Access, AuditEntry, Caller, Deps, ProcessApi, SourceUnavailable, Sourced,
    durable_idempotency_store, memory_idempotency_store, process_routes,
};
use spot_core::{Clock, SiteId, SystemClock};
use spot_http::api::idempotency::IdempotencyStore;
use spot_http::api::ports::process::{ProcessPortError, ProcessResult};
use spot_http::api::service::BoxFuture;
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, LeaseStore};
use tower::ServiceExt as _;

const HOST: &str = "erp.idem.test";

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 6;
    PgBackend::connect_unchecked(cfg).unwrap()
}

#[derive(Default)]
struct NullSubmit;
impl JobSubmit for NullSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

const WAITER: &str = r#"
name = "waiter"
app = "idem_test"
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

/// The caller is whoever the bearer token names (token = user id).
#[derive(Default)]
struct Everyone {
    users: Mutex<BTreeMap<String, RecordId>>,
}

impl Everyone {
    fn token(&self, name: &str) -> String {
        self.users
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_insert_with(RecordId::new_v7)
            .to_string()
    }
}

impl Access for Everyone {
    fn caller<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
    ) -> BoxFuture<'a, ProcessResult<Caller>> {
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
    fn supervises(&self, _principal: &Principal) -> bool {
        false
    }
    fn may_read<'a>(
        &'a self,
        _c: &'a Caller,
        _doctype: &'a str,
        _id: RecordId,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn assigned_cases<'a>(&'a self, _user: &'a str) -> Sourced<'a, Vec<String>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn is_party<'a>(&'a self, _user: &'a str, _case: &'a str) -> Sourced<'a, bool> {
        Box::pin(async { Ok::<bool, SourceUnavailable>(false) })
    }
}

/// The site: the process host and the Case table persist across "restarts".
struct Site {
    tenant: String,
    db_backend: PgBackend,
    idem_backend: PgBackend,
    process: Arc<ProcessHost>,
    definitions: Arc<PgDefinitionStore>,
    people: Arc<Everyone>,
    clock: Arc<dyn Clock>,
}

async fn site(db: &TestDb, tenant: &str) -> Site {
    let db_backend = backend(db, tenant);
    let pool = db_backend.pool();
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
            jiff::Timestamp::new(now().unix_timestamp(), 0).unwrap(),
        )
        .unwrap();
    let inner: Arc<dyn spot_bpm_engine::TimerPort> =
        Arc::new(spot_bpm_engine::InMemoryTimerPort::new());
    let timers = FencedTimerPort::new(leases, lease, inner);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let process = ProcessHost::build(
        SiteId::new(tenant).unwrap(),
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
    process
        .engine()
        .unwrap()
        .publish(&from_text(WAITER).unwrap())
        .unwrap();
    Site {
        tenant: tenant.to_owned(),
        db_backend,
        idem_backend: backend(db, &format!("{tenant}-idem")),
        process,
        definitions,
        people: Arc::new(Everyone::default()),
        clock,
    }
}

impl Site {
    /// A new port, a new store and a new router over the same database: a restart or a second
    /// node. `memory` selects the in-memory store (`allow_memory_idempotency`).
    async fn boot(&self, memory: bool) -> axum::Router {
        let idempotency: Arc<dyn IdempotencyStore> = if memory {
            memory_idempotency_store(&self.tenant, Arc::clone(&self.clock))
        } else {
            durable_idempotency_store(
                &self.tenant,
                &self.idem_backend,
                Arc::clone(&self.clock),
                ServerConfig::default().request_timeout,
            )
            .await
            .unwrap()
        };
        let port = Arc::new(ProcessApi::new(Deps {
            site: SiteId::new(&self.tenant).unwrap(),
            host: self.process.clone(),
            definitions: self.definitions.clone(),
            case_file: Arc::new(MemoryCaseFile::new()) as Arc<dyn CaseFileStore>,
            access: self.people.clone() as Arc<dyn Access>,
            directory: None,
            audit: Arc::new(|_entry: AuditEntry| {}),
            clock: Arc::clone(&self.clock),
        }));
        let sites = StaticSites::new()
            .add(HOST, SiteId::new(&self.tenant).unwrap())
            .add_site_name(SiteId::new(&self.tenant).unwrap());
        ServerBuilder::new(ServerConfig::default(), sites)
            .site_routes(process_routes(port, idempotency))
            .build()
    }

    async fn cases(&self) -> i64 {
        let client = self.db_backend.pool().get().await.unwrap();
        client
            .query_one(
                "SELECT count(*) FROM spot_bpm_case WHERE tenant = $1",
                &[&self.tenant],
            )
            .await
            .unwrap()
            .get(0)
    }
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    bytes: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_slice(&self.bytes).unwrap_or(Json::Null)
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn run_id(&self) -> String {
        self.json()["data"]["id"].as_str().unwrap_or("").to_owned()
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// `POST /api/v1/process/waiter/runs` as `token` with `Idempotency-Key: key`.
async fn start(router: &axum::Router, token: &str, key: &str, body: &Json) -> Reply {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/process/waiter/runs")
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", key)
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap()
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_replays_the_first_answer_and_starts_no_second_run() {
    let db = db_or_skip!();
    let s = site(&db, "idem-restart").await;
    let token = s.people.token("ann");
    let body = json!({"input": {}});

    let first_boot = s.boot(false).await;
    let first = start(&first_boot, &token, "k1", &body).await;
    assert_eq!(first.status, 201, "{}", first.text());
    assert_eq!(first.header("idempotency-replayed"), None);
    let run = first.run_id();
    assert!(!run.is_empty());
    assert_eq!(s.cases().await, 1);

    // A restart (or a second node): a new router over the same database.
    let second_boot = s.boot(false).await;
    let replay = start(&second_boot, &token, "k1", &body).await;
    assert_eq!(replay.status, 200, "{}", replay.text());
    assert_eq!(replay.header("idempotency-replayed"), Some("true"));
    assert_eq!(replay.run_id(), run, "the same Run id");
    assert_eq!(replay.json()["data"], first.json()["data"]);
    assert_eq!(s.cases().await, 1, "exactly one Run in spot_bpm_case");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_key_with_a_different_body_is_a_conflict_even_after_a_restart() {
    let db = db_or_skip!();
    let s = site(&db, "idem-conflict").await;
    let token = s.people.token("ann");
    let first_boot = s.boot(false).await;
    let first = start(&first_boot, &token, "k1", &json!({"input": {}})).await;
    assert_eq!(first.status, 201, "{}", first.text());
    let second_boot = s.boot(false).await;
    let other = start(&second_boot, &token, "k1", &json!({"input": {"a": 1}})).await;
    assert_eq!(other.status, 409, "{}", other.text());
    assert_eq!(other.code(), "api.idempotency_conflict");
    assert_eq!(s.cases().await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_identical_requests_make_one_run_and_one_replay() {
    let db = db_or_skip!();
    let s = site(&db, "idem-concurrent").await;
    let token = s.people.token("ann");
    let body = json!({"input": {}});
    // Two nodes sharing the database, each with its own store and router.
    let (a, b) = (s.boot(false).await, s.boot(false).await);
    let (x, y) = tokio::join!(
        start(&a, &token, "race", &body),
        start(&b, &token, "race", &body)
    );
    let mut statuses = [x.status, y.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 201], "{} / {}", x.text(), y.text());
    assert_eq!(x.run_id(), y.run_id(), "one Run");
    let replayed = [&x, &y]
        .iter()
        .filter(|r| r.header("idempotency-replayed") == Some("true"))
        .count();
    assert_eq!(replayed, 1);
    assert_eq!(s.cases().await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_store_answers_503_with_retry_after_and_starts_nothing() {
    let db = db_or_skip!();
    let s = site(&db, "idem-down").await;
    let token = s.people.token("ann");
    let router = s.boot(false).await;
    // The store's own pool goes away: its connections are gone, the Case table is untouched.
    s.idem_backend.pool().close();
    let reply = start(&router, &token, "k-down", &json!({"input": {}})).await;
    assert_eq!(reply.status, 503, "{}", reply.text());
    assert_eq!(reply.code(), "server.unavailable");
    assert!(reply.header("retry-after").is_some(), "{:?}", reply.headers);
    assert_eq!(s.cases().await, 0, "nothing was started");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_allow_memory_idempotency_a_restart_forgets_the_key() {
    let db = db_or_skip!();
    let s = site(&db, "idem-memory").await;
    let token = s.people.token("ann");
    let body = json!({"input": {}});
    let first_boot = s.boot(true).await;
    let first = start(&first_boot, &token, "k1", &body).await;
    assert_eq!(first.status, 201, "{}", first.text());
    let second_boot = s.boot(true).await;
    let again = start(&second_boot, &token, "k1", &body).await;
    assert_eq!(
        again.header("idempotency-replayed"),
        None,
        "memory forgot the key: this is not a replay"
    );
    // The engine's own key namespace still finds the one Run, so no second Run starts.
    assert_eq!(again.run_id(), first.run_id());
    assert_eq!(s.cases().await, 1);
}
