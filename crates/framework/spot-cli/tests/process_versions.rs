//! The process versions module over a real PostgreSQL and the real process engine host
//! (WP-BPM-09.T4; ADR-49, ADR-071, ADR-080).
//!
//! Every test needs a PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing `SKIPPED:`, when
//! it is unset (the convention of `tests/process.rs` and `tests/process_triggers.rs`). The tables,
//! the engine host and the definition store are real; who is calling is a small fake (`Grants`).
//! The port is driven directly: its HTTP shaping (idempotency, ETag, body cap, the byte-identical
//! `404`) belongs to `spot-http`'s `process_version` routes and their own tests.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use spot_app_api::document::{DocState, Document};
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_bpm_definitions::DefinitionStore;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{CaseStore, HandlerRegistry};
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture, SystemReader};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_triggers::{
    Fut, Meta, NoSecrets, Parts as TriggerParts, Settings as TriggerSettings, TracingAudit,
    TriggerHost, TriggerReload,
};
use spot_cli::services::process_versions::{Access, Caller, Parts, Settings, VersionHost};
use spot_core::{Clock, ManualClock};
use spot_document::engine::{MemoryPrincipals, PrincipalSource};
use spot_events_contract::{Actor, EventMeta, PayloadDocument};
use spot_http::api::ports::process_version::{
    MigrationMapping, MigrationRequest, MigrationState, PublishRequest, VersionPort,
    VersionPortError, VersionState,
};
use spot_http::api::service::{Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, LeaseStore};
use spot_storage_contract::{FeedPosition, NodeRef, OutboxEvent, OutboxRecord, Version};

const PUBLISHER: &str = "publisher";
const MIGRATOR: &str = "migrator";
const READER: &str = "reader";
const NOBODY: &str = "nobody";

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

// --- process texts -----------------------------------------------------------------------------

/// `start -> done`, or `start -> review -> done` for the second shape.
fn process_text(name: &str, review: bool) -> String {
    let (edges, extra) = if review {
        (
            "[[edges]]\nfrom = \"start\"\nto = \"review\"\n\n[[edges]]\nfrom = \"review\"\nto = \"done\"\n",
            "[[nodes]]\nid = \"review\"\nkind = \"logic\"\nlogic = \"join\"\n\n",
        )
    } else {
        ("[[edges]]\nfrom = \"start\"\nto = \"done\"\n", "")
    };
    format!(
        "name = \"{name}\"\napp = \"versions_test\"\nversion = 1\nstatus = \"published\"\n\n\
         [[nodes]]\nid = \"start\"\nkind = \"trigger\"\ntrigger = \"manual\"\n\n{extra}\
         [[nodes]]\nid = \"done\"\nkind = \"logic\"\nlogic = \"join\"\nterminal = true\n\n{edges}"
    )
}

/// `start -> make (a Document node) -> done`, optionally acting as a service principal.
fn doc_process(name: &str, principal: Option<&str>) -> String {
    let principal = principal.map_or(String::new(), |p| format!("principal = \"{p}\"\n"));
    format!(
        "name = \"{name}\"\napp = \"versions_test\"\nversion = 1\nstatus = \"published\"\n\n\
         [[nodes]]\nid = \"start\"\nkind = \"trigger\"\ntrigger = \"manual\"\n\n\
         [[nodes]]\nid = \"make\"\nkind = \"document\"\naction = \"create\"\ndoctype = \"note\"\n{principal}\n\
         [[nodes]]\nid = \"done\"\nkind = \"logic\"\nlogic = \"join\"\nterminal = true\n\n\
         [[edges]]\nfrom = \"start\"\nto = \"make\"\n\n[[edges]]\nfrom = \"make\"\nto = \"done\"\n"
    )
}

fn v1(name: &str) -> String {
    process_text(name, false)
}

fn v2(name: &str) -> String {
    process_text(name, true)
}

// --- fakes -------------------------------------------------------------------------------------

struct NullSubmit;

impl JobSubmit for NullSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

/// Who is calling: a token maps to a user and the permissions that user holds. `broken` makes the
/// permission source fail (every `holds` answers false).
#[derive(Default)]
struct Grants {
    tokens: Mutex<HashMap<String, (String, BTreeSet<String>)>>,
    broken: AtomicBool,
}

impl Grants {
    fn with(users: &[(&str, &[&str])]) -> Arc<Self> {
        let grants = Arc::new(Self::default());
        for (user, perms) in users {
            grants.tokens.lock().unwrap().insert(
                (*user).to_owned(),
                (
                    (*user).to_owned(),
                    perms.iter().map(|p| (*p).to_owned()).collect(),
                ),
            );
        }
        grants
    }
}

impl Access for Grants {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> Fut<'a, Option<Caller>> {
        Box::pin(async move {
            let token = &ctx.credential.as_ref()?.token;
            let (user, _) = self.tokens.lock().unwrap().get(token).cloned()?;
            Some(Caller {
                principal: Principal::user(&user, Vec::new()),
                user,
            })
        })
    }

    fn holds(&self, caller: &Caller, permission: &str) -> bool {
        if self.broken.load(Ordering::SeqCst) {
            return false;
        }
        let tokens = self.tokens.lock().unwrap();
        tokens
            .get(&caller.user)
            .is_some_and(|(_, perms)| perms.contains(permission))
    }
}

fn users() -> Arc<Grants> {
    Grants::with(&[
        (PUBLISHER, &["process_publish"]),
        (MIGRATOR, &["process_migrate"]),
        (READER, &["process_read_versions"]),
        (NOBODY, &[]),
    ])
}

// --- the site under test -----------------------------------------------------------------------

fn backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 6;
    PgBackend::connect_unchecked(cfg).unwrap()
}

fn to_jiff(at: OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::new(at.unix_timestamp(), 0).unwrap()
}

struct Site {
    tenant: String,
    pool: deadpool_postgres::Pool,
    process: Arc<ProcessHost>,
    definitions: Arc<PgDefinitionStore>,
    clock: Arc<ManualClock>,
    grants: Arc<Grants>,
}

async fn site(db: &TestDb, tenant: &str) -> Site {
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
            to_jiff(at(1_700_000_000)),
        )
        .unwrap();
    let inner: Arc<dyn spot_bpm_engine::TimerPort> =
        Arc::new(spot_bpm_engine::InMemoryTimerPort::new());
    let timers = FencedTimerPort::new(leases, lease, inner);
    let clock = Arc::new(ManualClock::new(at(1_700_000_000)));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let process = ProcessHost::build(
        SiteId::new(tenant.replace('_', "-")).unwrap(),
        store,
        timers,
        HandlerRegistry::logic_default(),
        definitions.clone(),
        Arc::new(NullSubmit),
        dyn_clock,
    );
    Site {
        tenant: tenant.to_owned(),
        pool,
        process,
        definitions,
        clock,
        grants: users(),
    }
}

impl Site {
    /// A module host ("a node") over this site's stores; each has its own lease holder.
    async fn node(&self, batch: u32) -> Arc<VersionHost> {
        let host = VersionHost::new(Parts {
            site: SiteId::new(self.tenant.replace('_', "-")).unwrap(),
            pool: self.pool.clone(),
            definitions: self.definitions.clone(),
            process: self.process.clone(),
            access: self.grants.clone(),
            guard: None,
            triggers: None,
            trigger_settings: TriggerSettings::default(),
            clock: self.clock.clone(),
            settings: Settings {
                batch,
                ..Settings::default()
            },
        });
        host.prepare().await.unwrap();
        host
    }

    fn ctx(&self, user: &str, key: Option<&str>) -> RequestCtx {
        RequestCtx {
            site: SiteId::new(self.tenant.replace('_', "-")).unwrap(),
            credential: Some(Credential::new(user)),
            idempotency_key: key.map(str::to_owned),
            language: None,
            request_id: RequestId::generate(),
            peer: None,
        }
    }

    async fn audit_rows(&self, outcome: &str) -> i64 {
        let client = self.pool.get().await.unwrap();
        client
            .query_one(
                "SELECT count(*) FROM spot_bpm_version_audit WHERE tenant = $1 AND outcome = $2",
                &[&self.tenant.replace('_', "-"), &outcome],
            )
            .await
            .unwrap()
            .get(0)
    }

    async fn dag_version(&self, case: &str) -> i32 {
        let client = self.pool.get().await.unwrap();
        client
            .query_one(
                "SELECT dag_version FROM spot_bpm_case WHERE tenant = $1 AND case_id = $2",
                &[&self.tenant, &case],
            )
            .await
            .unwrap()
            .get(0)
    }
}

fn request(text: String, expected_previous: Option<u32>) -> PublishRequest {
    PublishRequest {
        definition: text,
        note: "a note".to_owned(),
        expected_previous,
    }
}

fn skipped(db: &Option<TestDb>) -> bool {
    if db.is_none() {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
    }
    db.is_none()
}

async fn start_run(site: &Site, process: &str, version: Option<u32>, key: &str) -> String {
    site.process
        .start_run(process, version, json!({}), None, key, at(1_700_000_000))
        .await
        .unwrap()
        .to_string()
}

// --- publish, list, get, the head --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_numbers_versions_and_a_stale_expected_previous_is_refused() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "pub_numbers").await;
    let host = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k1"));
    let first = host
        .publish(&ctx, "order", request(v1("order"), None))
        .await
        .unwrap();
    assert_eq!(first.version.version, 1);
    assert_eq!(first.version.state, VersionState::Published);
    assert_eq!(first.version.published_by.as_deref(), Some(PUBLISHER));
    let second = host
        .publish(&ctx, "order", request(v2("order"), Some(1)))
        .await
        .unwrap();
    assert_eq!(second.version.version, 2);
    assert_ne!(first.hash, second.hash);
    // The head is 2 now: expecting 1 again is stale, and nothing is stored.
    let stale = host
        .publish(&ctx, "order", request(v1("order"), Some(1)))
        .await;
    assert_eq!(stale.unwrap_err(), VersionPortError::VersionConflict);
    let page = host.list_versions(&ctx, "order", None, 50).await.unwrap();
    let numbers: Vec<u32> = page.items.iter().map(|v| v.version).collect();
    assert_eq!(numbers, vec![2, 1]);
    // The engine serves the newest published version to new starts.
    let latest = s
        .process
        .engine()
        .unwrap()
        .processes()
        .latest("order")
        .unwrap();
    assert_eq!(latest.number(), 2);
    let detail = host.get_version(&ctx, "order", 1).await.unwrap();
    assert_eq!(detail.hash, first.hash);
    assert_eq!(detail.definition, v1("order"));
    // A refused publish is audited with its rule.
    assert_eq!(s.audit_rows("process.version_conflict").await, 1);
    assert_eq!(s.audit_rows("ok").await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_publishes_of_one_head_make_one_version_and_one_conflict() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "pub_race").await;
    let host = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k"));
    host.publish(&ctx, "order", request(v1("order"), None))
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        host.publish(&ctx, "order", request(v2("order"), Some(1))),
        host.publish(&ctx, "order", request(v2("order"), Some(1)))
    );
    let wins = [&a, &b].iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "{a:?} {b:?}");
    let loser = if a.is_err() { a } else { b };
    assert_eq!(loser.unwrap_err(), VersionPortError::VersionConflict);
    assert_eq!(
        s.definitions
            .latest_published("order")
            .await
            .unwrap()
            .unwrap()
            .version,
        2
    );
    // The loser never reached the live registry: exactly the stored winner is live, and nothing
    // above it.
    let engine = s.process.engine().unwrap();
    let stored = s.definitions.get("order", 2).await.unwrap().unwrap();
    let live = engine
        .processes()
        .get("order", 2)
        .expect("the winner is live");
    // The store's content hash is of the stored text and the DAG's is of the compiled graph: two
    // hashes, so what ties the live version to the stored one is its number and its text.
    assert_eq!(live.number(), stored.version);
    assert_eq!(stored.version, 2);
    assert!(engine.processes().get("order", 3).is_none());
    assert_eq!(engine.processes().latest("order").unwrap().number(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_definition_is_refused_with_node_ids_only_and_stores_nothing() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "pub_invalid").await;
    let host = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k"));
    let cycle = format!(
        "{}\n[[edges]]\nfrom = \"done\"\nto = \"start\"\n",
        v1("order")
    );
    let unreachable = v1("order").replace(
        "[[edges]]",
        "[[nodes]]\nid = \"island\"\nkind = \"logic\"\nlogic = \"join\"\n\n[[edges]]",
    );
    let handler_missing = doc_process("order", None);
    let service_principal = doc_process("order", Some("billing_bot"));
    let secret_text = "SECRET-TOKEN-123 not a process";
    for (label, text) in [
        ("cycle", cycle),
        ("unreachable", unreachable),
        ("handler_missing", handler_missing),
        ("service_principal", service_principal),
        ("not_a_process", secret_text.to_owned()),
    ] {
        let refused = host
            .publish(&ctx, "order", request(text, None))
            .await
            .unwrap_err();
        match (label, &refused) {
            ("service_principal", VersionPortError::PublishDenied) => {}
            (_, VersionPortError::DefinitionInvalid(problems)) => {
                assert!(!problems.is_empty(), "{label}");
                if label == "handler_missing" {
                    assert!(
                        problems
                            .iter()
                            .any(|p| p.node.as_deref() == Some("make")
                                && p.kind == "handler_missing")
                    );
                }
                for p in problems {
                    // A node id from the document or none, and a closed kind token.
                    assert!(
                        p.node
                            .as_deref()
                            .is_none_or(|n| ["start", "done", "island", "make"].contains(&n))
                    );
                    assert!(p.kind.chars().all(|c| c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || c == '_'
                        || c == '.'));
                }
            }
            other => panic!("{label}: {other:?}"),
        }
        // Nothing of the text is ever echoed.
        assert!(!format!("{refused:?}").contains("SECRET-TOKEN"));
    }
    assert!(
        s.definitions
            .latest_published("order")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(s.audit_rows("process.definition_invalid").await, 4);
    assert_eq!(s.audit_rows("process.publish_denied").await, 1);
}

// --- permissions and isolation ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_permission_every_operation_is_the_same_not_found_as_for_an_unknown_process() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "perm_denied").await;
    let host = s.node(100).await;
    host.publish(
        &s.ctx(PUBLISHER, Some("k")),
        "order",
        request(v1("order"), None),
    )
    .await
    .unwrap();
    let nobody = s.ctx(NOBODY, Some("k"));
    let publisher = s.ctx(PUBLISHER, Some("k"));
    let missing = host
        .list_versions(&publisher, "no_such", None, 50)
        .await
        .unwrap_err();
    let denied = host
        .list_versions(&nobody, "order", None, 50)
        .await
        .unwrap_err();
    assert_eq!(missing, VersionPortError::NotFound);
    assert_eq!(denied, missing);
    assert_eq!(
        host.get_version(&nobody, "order", 1).await.unwrap_err(),
        missing
    );
    assert_eq!(
        host.get_version(&publisher, "order", 9).await.unwrap_err(),
        missing
    );
    assert_eq!(host.retire(&nobody, "order", 1).await.unwrap_err(), missing);
    assert_eq!(
        host.publish(&nobody, "order", request(v2("order"), None))
            .await
            .unwrap_err(),
        missing
    );
    let migration = MigrationRequest {
        to: 2,
        mapping: None,
    };
    assert_eq!(
        host.plan_migration(&nobody, "order", 1, migration)
            .await
            .unwrap_err(),
        missing
    );
    // The read permission lists and gets but does not publish; a publisher does not migrate.
    let reader = s.ctx(READER, Some("k"));
    assert_eq!(
        host.list_versions(&reader, "order", None, 50)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        host.publish(&reader, "order", request(v2("order"), None))
            .await
            .unwrap_err(),
        missing
    );
    let migration = MigrationRequest {
        to: 2,
        mapping: None,
    };
    assert_eq!(
        host.plan_migration(&publisher, "order", 1, migration)
            .await
            .unwrap_err(),
        missing
    );
    // An unauthenticated request and a request without a credential are the same answer.
    let mut anonymous = s.ctx(PUBLISHER, None);
    anonymous.credential = None;
    assert_eq!(
        host.list_versions(&anonymous, "order", None, 50)
            .await
            .unwrap_err(),
        missing
    );
    assert!(s.audit_rows("process.not_found").await >= 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_source_that_fails_denies_everything() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "perm_broken").await;
    let host = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k"));
    host.publish(&ctx, "order", request(v1("order"), None))
        .await
        .unwrap();
    s.grants.broken.store(true, Ordering::SeqCst);
    assert_eq!(
        host.list_versions(&ctx, "order", None, 50)
            .await
            .unwrap_err(),
        VersionPortError::NotFound
    );
    let published = host
        .publish(&ctx, "order", request(v2("order"), None))
        .await;
    assert_eq!(published.unwrap_err(), VersionPortError::NotFound);
    assert_eq!(
        s.definitions
            .latest_published("order")
            .await
            .unwrap()
            .unwrap()
            .version,
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sites_are_isolated_in_versions_runs_and_migrations() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let (a, b) = (site(&db, "iso_a").await, site(&db, "iso_b").await);
    let (host_a, host_b) = (a.node(100).await, b.node(100).await);
    host_a
        .publish(
            &a.ctx(PUBLISHER, Some("k")),
            "order",
            request(v1("order"), None),
        )
        .await
        .unwrap();
    // Site B has no such process, by list, by get and by a request addressed to site A.
    let in_b = b.ctx(PUBLISHER, Some("k"));
    assert_eq!(
        host_b
            .list_versions(&in_b, "order", None, 50)
            .await
            .unwrap_err(),
        VersionPortError::NotFound
    );
    assert_eq!(
        host_b.get_version(&in_b, "order", 1).await.unwrap_err(),
        VersionPortError::NotFound
    );
    let for_a = a.ctx(PUBLISHER, Some("k"));
    assert_eq!(
        host_b
            .list_versions(&for_a, "order", None, 50)
            .await
            .unwrap_err(),
        VersionPortError::NotFound
    );
    // Runs of A are not counted, listed or moved for B's process of the same name.
    host_b
        .publish(&in_b, "order", request(v1("order"), None))
        .await
        .unwrap();
    host_b
        .publish(&in_b, "order", request(v2("order"), Some(1)))
        .await
        .unwrap();
    let case = start_run(&a, "order", Some(1), "only-a").await;
    let migration = || MigrationRequest {
        to: 2,
        mapping: None,
    };
    let plan = host_b
        .plan_migration(&b.ctx(MIGRATOR, None), "order", 1, migration())
        .await
        .unwrap();
    assert_eq!((plan.movable, plan.unmoved_total), (0, 0));
    assert_eq!(a.dag_version(&case).await, 1);
    // The audit rows of one site are not the other's.
    assert_eq!(a.audit_rows("ok").await, 1);
}

// --- retire and pinning ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retire_stops_new_starts_and_refuses_a_second_retire() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "retire").await;
    let host = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k"));
    host.publish(&ctx, "order", request(v1("order"), None))
        .await
        .unwrap();
    host.publish(&ctx, "order", request(v2("order"), Some(1)))
        .await
        .unwrap();
    let retired = host.retire(&ctx, "order", 1).await.unwrap();
    assert_eq!(retired.state, VersionState::Retired);
    assert_eq!(
        host.retire(&ctx, "order", 1).await.unwrap_err(),
        VersionPortError::VersionNotPublished
    );
    assert_eq!(
        host.retire(&ctx, "order", 7).await.unwrap_err(),
        VersionPortError::NotFound
    );
    // Retiring the only published version leaves no version a new Run can start on.
    host.retire(&ctx, "order", 2).await.unwrap();
    let engine = s.process.engine().unwrap();
    assert!(engine.processes().latest("order").is_none());
    let started = s
        .process
        .start_run("order", None, json!({}), None, "late", at(1_700_000_000))
        .await;
    assert!(started.is_err());
    assert_eq!(s.audit_rows("process.version_not_published").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_case_stays_on_its_version_after_a_publish_and_a_new_start_uses_the_new_one() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "pinning").await;
    let host = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k"));
    host.publish(&ctx, "order", request(v1("order"), None))
        .await
        .unwrap();
    let old = start_run(&s, "order", None, "old-run").await;
    host.publish(&ctx, "order", request(v2("order"), Some(1)))
        .await
        .unwrap();
    let new = start_run(&s, "order", None, "new-run").await;
    assert_eq!(s.dag_version(&old).await, 1);
    assert_eq!(s.dag_version(&new).await, 2);
    // The Case the engine loads still carries the pin it started on.
    let engine = s.process.engine().unwrap();
    let id = spot_bpm_engine::CaseId::new(&old).unwrap();
    let record = engine.store().load(&id).await.unwrap().unwrap();
    assert_eq!(record.run.dag_version, 1);
    // The list counts the Runs pinned to each version.
    let page = host.list_versions(&ctx, "order", None, 50).await.unwrap();
    let pins: BTreeMap<u32, u32> = page
        .items
        .iter()
        .map(|v| (v.version, v.running_runs))
        .collect();
    assert_eq!(pins, BTreeMap::from([(1, 1), (2, 1)]));
}

// --- migration ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_migration_plan_counts_and_skips_and_changes_nothing() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "plan").await;
    let host = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k"));
    host.publish(&ctx, "order", request(v1("order"), None))
        .await
        .unwrap();
    host.publish(&ctx, "order", request(v2("order"), Some(1)))
        .await
        .unwrap();
    let cases = [
        start_run(&s, "order", Some(1), "a").await,
        start_run(&s, "order", Some(1), "b").await,
    ];
    // One Run is mid-node: a migration never forces it.
    let client = s.pool.get().await.unwrap();
    client
        .execute(
            "UPDATE spot_bpm_case SET nodes = jsonb_set(nodes, '{start,status}', '\"running\"') \
             WHERE tenant = $1 AND case_id = $2",
            &[&s.tenant, &cases[1]],
        )
        .await
        .unwrap();
    let migrator = s.ctx(MIGRATOR, None);
    let plan = host
        .plan_migration(
            &migrator,
            "order",
            1,
            MigrationRequest {
                to: 2,
                mapping: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (plan.from, plan.to, plan.movable, plan.unmoved_total),
        (1, 2, 1, 1)
    );
    assert_eq!(plan.unmoved.len(), 1);
    assert_eq!(plan.unmoved[0].run, cases[1]);
    assert_eq!(plan.unmoved[0].reason, "bpm_versioning.case_mid_node");
    // A dry run writes no migration record and moves nothing.
    let records: i64 = client
        .query_one(
            "SELECT count(*) FROM spot_bpm_migration WHERE tenant = $1",
            &[&s.tenant],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(records, 0);
    assert_eq!(s.dag_version(&cases[0]).await, 1);
    // The refusals: same version, an unpublished or unknown target, a mapping to a node the target lacks.
    let bad = |to, mapping| MigrationRequest { to, mapping };
    assert_eq!(
        host.plan_migration(&migrator, "order", 1, bad(1, None))
            .await
            .unwrap_err(),
        VersionPortError::MigrationInvalid
    );
    assert_eq!(
        host.plan_migration(&migrator, "order", 1, bad(9, None))
            .await
            .unwrap_err(),
        VersionPortError::VersionNotPublished
    );
    let mut mapping = MigrationMapping::default();
    mapping
        .nodes
        .insert("start".to_owned(), "no_such_node".to_owned());
    let refused = host
        .plan_migration(&migrator, "order", 1, bad(2, Some(mapping)))
        .await;
    assert_eq!(refused.unwrap_err(), VersionPortError::MigrationInvalid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_migration_resumes_after_a_restart_and_moves_each_run_exactly_once() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "resume").await;
    let first = s.node(2).await;
    let ctx = s.ctx(PUBLISHER, Some("k"));
    first
        .publish(&ctx, "order", request(v1("order"), None))
        .await
        .unwrap();
    first
        .publish(&ctx, "order", request(v2("order"), Some(1)))
        .await
        .unwrap();
    let mut cases = Vec::new();
    for key in ["r1", "r2", "r3"] {
        cases.push(start_run(&s, "order", Some(1), key).await);
    }
    let client = s.pool.get().await.unwrap();
    client
        .execute(
            "UPDATE spot_bpm_case SET nodes = jsonb_set(nodes, '{start,status}', '\"running\"') \
             WHERE tenant = $1 AND case_id = $2",
            &[&s.tenant, &cases[2]],
        )
        .await
        .unwrap();
    let migrator = s.ctx(MIGRATOR, Some("start-1"));
    let request_to_2 = || MigrationRequest {
        to: 2,
        mapping: None,
    };
    let id = first
        .start_migration(&migrator, "order", 1, request_to_2())
        .await
        .unwrap();
    // The same key again is the same migration and starts nothing.
    assert_eq!(
        first
            .start_migration(&migrator, "order", 1, request_to_2())
            .await
            .unwrap(),
        id
    );
    assert!(id.len() >= 16 && id.len() <= 64);
    // Pass one moves a batch of two, then the node "dies": its lease is still held, so a second
    // node finds nothing to claim until the lease lapses.
    assert!(first.migrate_pass().await.unwrap());
    let second = s.node(2).await;
    assert!(!second.migrate_pass().await.unwrap());
    let mid = first.get_migration(&migrator, "order", &id).await.unwrap();
    assert_eq!(mid.state, MigrationState::Running);
    s.clock.advance(Duration::from_secs(3600));
    while second.migrate_pass().await.unwrap() {}
    let done = second.get_migration(&migrator, "order", &id).await.unwrap();
    assert_eq!(done.state, MigrationState::Completed);
    assert_eq!((done.moved, done.skipped, done.failed), (2, 1, 0));
    assert!(done.ended_at.is_some());
    // Exactly the two idle Runs moved; the mid-node one stayed on version 1.
    assert_eq!(s.dag_version(&cases[0]).await, 2);
    assert_eq!(s.dag_version(&cases[1]).await, 2);
    assert_eq!(s.dag_version(&cases[2]).await, 1);
    let audits: i64 = client
        .query_one(
            "SELECT count(*) FROM spot_bpm_migration_audit WHERE tenant = $1",
            &[&s.tenant],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(audits, 2);
    // Another process's id, or another site's caller, is the same not-found.
    let other = second.get_migration(&migrator, "other", &id).await;
    assert_eq!(other.unwrap_err(), VersionPortError::NotFound);
    let nobody = s.ctx(NOBODY, None);
    assert_eq!(
        second
            .get_migration(&nobody, "order", &id)
            .await
            .unwrap_err(),
        VersionPortError::NotFound
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_migration_never_runs_to_a_target_that_is_not_published() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "target").await;
    let host = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k"));
    host.publish(&ctx, "order", request(v1("order"), None))
        .await
        .unwrap();
    host.publish(&ctx, "order", request(v2("order"), Some(1)))
        .await
        .unwrap();
    host.retire(&ctx, "order", 2).await.unwrap();
    let migrator = s.ctx(MIGRATOR, Some("k2"));
    let started = host
        .start_migration(
            &migrator,
            "order",
            1,
            MigrationRequest {
                to: 2,
                mapping: None,
            },
        )
        .await;
    assert_eq!(started.unwrap_err(), VersionPortError::VersionNotPublished);
    assert_eq!(s.audit_rows("process.version_not_published").await, 1);
}

// --- publish and retire reload the triggers at once (WP-BPM-03.T5) -----------------------------

/// A process started by a document event on `doctype`.
fn event_process(name: &str, doctype: &str) -> String {
    format!(
        "name = \"{name}\"\napp = \"versions_test\"\nversion = 1\nstatus = \"published\"\n\n\
         [[nodes]]\nid = \"start\"\nkind = \"trigger\"\ntrigger = \"document_event\"\n\
         doctype = \"{doctype}\"\nevent = \"post\"\n\n\
         [[nodes]]\nid = \"done\"\nkind = \"logic\"\nlogic = \"join\"\nterminal = true\n\n\
         [[edges]]\nfrom = \"start\"\nto = \"done\"\n"
    )
}

/// The platform reader: the documents the events are about.
#[derive(Default)]
struct Reader(Mutex<HashMap<(String, String), Document>>);

impl Reader {
    /// A posted document of `doctype`, and the id to name it by.
    fn posted(&self, doctype: &str) -> RecordId {
        let id = RecordId::new_v7();
        let mut doc = Document::new(Name::new(doctype).unwrap(), id);
        doc.set(Name::new("title").unwrap(), Value::Text("w".to_owned()));
        doc.mark_saved(Version(1), Some(DocState::Posted));
        self.0
            .lock()
            .unwrap()
            .insert((doctype.to_owned(), id.to_string()), doc);
        id
    }
}

impl SystemReader for Reader {
    fn read<'a>(&'a self, doctype: &'a str, name: &'a str) -> PortFuture<'a, Option<Document>> {
        Box::pin(async move {
            let key = (doctype.to_owned(), name.to_owned());
            Ok(self.0.lock().unwrap().get(&key).cloned())
        })
    }
}

/// Both doctypes have a lifecycle; no permission engine (the events are the platform's).
struct Stateful;

impl Meta for Stateful {
    fn stateful(&self) -> Option<BTreeSet<String>> {
        Some(
            ["widget", "gadget"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        )
    }
    fn engine(&self) -> Option<spot_permissions::PermissionEngine> {
        None
    }
}

/// A `doc_posted` event of the platform about `doc`, at feed position `seq`.
fn posted_event(seq: u64, doctype: &str, doc: &RecordId) -> OutboxRecord {
    let payload = PayloadDocument {
        meta: EventMeta {
            occurred_at: at(1_700_000_000),
            actor: Actor::System {
                component: Name::new("test").unwrap(),
            },
            command_id: None,
            causation_id: None,
            correlation_id: RecordId::new_v7(),
            trace_id: None,
        },
        data: json!({}),
    };
    OutboxRecord {
        position: FeedPosition::new(1, seq),
        transaction: RecordId::new_v7(),
        ordinal: 0,
        event: OutboxEvent {
            id: RecordId::new_v7(),
            topic: Name::new("doc_posted").unwrap(),
            subject: NodeRef {
                entity: Name::new(doctype).unwrap(),
                id: *doc,
            },
            subject_version: Some(Version(1)),
            schema_version: 1,
            payload: serde_json::to_string(&payload).unwrap(),
        },
    }
}

/// The trigger port a test can make fail: it reloads the real intake unless told not to.
struct Switch {
    inner: Arc<TriggerHost>,
    fail: AtomicBool,
    calls: AtomicU32,
}

impl TriggerReload for Switch {
    fn reload_now(&self) -> Fut<'_, spot_cli::Result<usize>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(spot_cli::Error(
                    "reading the process definitions failed: unavailable".to_owned(),
                ));
            }
            self.inner.reload_now().await
        })
    }
}

/// A site with a real trigger intake (the platform reader and metadata are fakes) and a versions
/// host that reloads it through the port.
struct Wired {
    site: Site,
    reader: Arc<Reader>,
    triggers: Arc<TriggerHost>,
    switch: Arc<Switch>,
    versions: Arc<VersionHost>,
    seq: AtomicU32,
}

impl Site {
    /// A trigger intake over this site's stores (the intake's own seams are fakes).
    async fn trigger_host(&self, reader: Arc<Reader>) -> Arc<TriggerHost> {
        let principals: Arc<dyn PrincipalSource> = Arc::new(MemoryPrincipals::new());
        let host = TriggerHost::new(TriggerParts {
            site: SiteId::new(self.tenant.replace('_', "-")).unwrap(),
            clock: self.clock.clone(),
            settings: TriggerSettings::default(),
            process: self.process.clone(),
            definitions: self.definitions.clone(),
            submit: Arc::new(NullSubmit),
            reader,
            principals,
            meta: Arc::new(Stateful),
            secrets: Arc::new(NoSecrets),
            audit: Arc::new(TracingAudit),
            pool: self.pool.clone(),
        });
        host.prepare().await.unwrap();
        host.reload().await.unwrap();
        host
    }
}

async fn wired(db: &TestDb, tenant: &str) -> Wired {
    let site = site(db, tenant).await;
    let reader = Arc::new(Reader::default());
    let triggers = site.trigger_host(reader.clone()).await;
    let switch = Arc::new(Switch {
        inner: triggers.clone(),
        fail: AtomicBool::new(false),
        calls: AtomicU32::new(0),
    });
    let versions = VersionHost::new(Parts {
        site: SiteId::new(site.tenant.replace('_', "-")).unwrap(),
        pool: site.pool.clone(),
        definitions: site.definitions.clone(),
        process: site.process.clone(),
        access: site.grants.clone(),
        guard: None,
        triggers: Some(switch.clone()),
        trigger_settings: TriggerSettings::default(),
        clock: site.clock.clone(),
        settings: Settings::default(),
    });
    versions.prepare().await.unwrap();
    Wired {
        site,
        reader,
        triggers,
        switch,
        versions,
        seq: AtomicU32::new(0),
    }
}

impl Wired {
    /// Cases the intake started so far.
    fn started(&self) -> u64 {
        self.triggers.stats().started
    }

    /// Delivers one fresh `doc_posted` event on a new document of `doctype` to the intake, with
    /// no other reload in between: what the intake does is what it knows right now.
    async fn event(&self, doctype: &str) {
        let doc = self.reader.posted(doctype);
        let seq = u64::from(self.seq.fetch_add(1, Ordering::SeqCst)) + 1;
        let record = posted_event(seq, doctype, &doc);
        self.triggers.ingest(&[record]).await.unwrap();
    }
}

#[tokio::test]
async fn a_published_trigger_fires_for_the_next_event_without_waiting_for_the_pass() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    // The database lives as long as the test: a temporary would drop it after this line.
    let db = db.unwrap();
    let w = wired(&db, "trig_publish").await;
    let ctx = w.site.ctx(PUBLISHER, Some("k1"));
    // Nothing is published: an event starts nothing.
    w.event("widget").await;
    assert_eq!(w.started(), 0);
    // The publish returns; the very next event starts a Case (no tick, no manual reload).
    w.versions
        .publish(
            &ctx,
            "ev_proc",
            request(event_process("ev_proc", "widget"), None),
        )
        .await
        .unwrap();
    assert_eq!(w.switch.calls.load(Ordering::SeqCst), 1);
    w.event("widget").await;
    assert_eq!(w.started(), 1);
    // Version 2 moves the trigger to another doctype: the old one stops, the new one starts, at once.
    w.versions
        .publish(
            &ctx,
            "ev_proc",
            request(event_process("ev_proc", "gadget"), Some(1)),
        )
        .await
        .unwrap();
    w.event("widget").await;
    assert_eq!(w.started(), 1, "the replaced trigger no longer fires");
    w.event("gadget").await;
    assert_eq!(w.started(), 2);
}

#[tokio::test]
async fn retiring_the_only_published_version_turns_its_triggers_off_at_once() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    // The database lives as long as the test: a temporary would drop it after this line.
    let db = db.unwrap();
    let w = wired(&db, "trig_retire").await;
    let ctx = w.site.ctx(PUBLISHER, Some("k1"));
    w.versions
        .publish(
            &ctx,
            "ev_proc",
            request(event_process("ev_proc", "widget"), None),
        )
        .await
        .unwrap();
    w.event("widget").await;
    assert_eq!(w.started(), 1);
    w.versions.retire(&ctx, "ev_proc", 1).await.unwrap();
    assert_eq!(
        w.switch.calls.load(Ordering::SeqCst),
        2,
        "publish and retire"
    );
    w.event("widget").await;
    assert_eq!(w.started(), 1, "a retired-only process starts nothing");
}

#[tokio::test]
async fn with_the_triggers_module_off_a_publish_succeeds_and_the_pass_picks_it_up() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    // The database lives as long as the test: a temporary would drop it after this line.
    let db = db.unwrap();
    let s = site(&db, "trig_off").await;
    let reader = Arc::new(Reader::default());
    let triggers = s.trigger_host(reader.clone()).await;
    // The versions host has no trigger port (the intake is not provided on this site).
    let versions = s.node(100).await;
    let ctx = s.ctx(PUBLISHER, Some("k1"));
    let published = versions
        .publish(
            &ctx,
            "ev_proc",
            request(event_process("ev_proc", "widget"), None),
        )
        .await
        .unwrap();
    assert_eq!(published.version.version, 1);
    assert_eq!(s.audit_rows("trigger_reload_failed").await, 0);
    let doc = reader.posted("widget");
    triggers
        .ingest(&[posted_event(1, "widget", &doc)])
        .await
        .unwrap();
    assert_eq!(triggers.stats().started, 0, "stale until the pass");
    // The intake's own pass (the same reload) brings it in.
    triggers.reload().await.unwrap();
    let doc = reader.posted("widget");
    triggers
        .ingest(&[posted_event(2, "widget", &doc)])
        .await
        .unwrap();
    assert_eq!(triggers.stats().started, 1);
}

#[tokio::test]
async fn a_failing_trigger_reload_keeps_the_stored_version_and_the_old_triggers_and_is_audited() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    // The database lives as long as the test: a temporary would drop it after this line.
    let db = db.unwrap();
    let w = wired(&db, "trig_fail").await;
    let ctx = w.site.ctx(PUBLISHER, Some("k1"));
    w.versions
        .publish(
            &ctx,
            "ev_proc",
            request(event_process("ev_proc", "widget"), None),
        )
        .await
        .unwrap();
    w.event("widget").await;
    assert_eq!(w.started(), 1);
    // The reload fails while version 2 (a trigger on another doctype) is published.
    w.switch.fail.store(true, Ordering::SeqCst);
    let second = w
        .versions
        .publish(
            &ctx,
            "ev_proc",
            request(event_process("ev_proc", "gadget"), Some(1)),
        )
        .await
        .unwrap();
    assert_eq!(
        second.version.version, 2,
        "the publish is stored and not rolled back"
    );
    let page = w
        .versions
        .list_versions(&ctx, "ev_proc", None, 50)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(w.site.audit_rows("trigger_reload_failed").await, 1);
    // The trigger set in force is the old one.
    w.event("gadget").await;
    assert_eq!(w.started(), 1);
    w.event("widget").await;
    assert_eq!(w.started(), 2);
    // The worker's retry (the module's own `reload`) brings the new set in once the intake answers.
    w.switch.fail.store(false, Ordering::SeqCst);
    assert!(w.versions.reload().await);
    w.event("widget").await;
    assert_eq!(w.started(), 2);
    w.event("gadget").await;
    assert_eq!(w.started(), 3);
}
