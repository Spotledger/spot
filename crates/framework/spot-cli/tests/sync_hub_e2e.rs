//! The hub's parts end to end (WP-RTB-02.T13): the enrolment flow from a one-time code to a node
//! the directory authenticates, the endpoint over the real router, revocation seen at the next
//! heartbeat, restart keeping the directory, and tenants staying apart.
//!
//! The pieces that need no database run everywhere (settings, the fail-closed registration, the
//! enrolment flow over the in-memory store). The PostgreSQL tests need `SPOT_TEST_PG_URL` and are
//! skipped, with a printed reason, when it is unset (`SPOT_REQUIRE_PG=1` makes that a failure).
//!
//! Not covered here, and owed to the bundle run because it needs a certificate authority for the
//! hub's own TLS identity and a running listener: a real `spot serve` with a TLS client doing hello,
//! pull and push; a bind failure making the site not ready (`tests/sync_hub_host.rs` covers the
//! taken port of the listener itself).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use spot_cli::services::SiteCtx;
use spot_cli::services::node_role::{ActiveRole, Role};
use spot_cli::services::sync_hub_parts::{
    self, EnrolRedeemer, Inputs, PART_MISSING, RelayAssembly, Settings, refusal_of,
};
use spot_core::{Clock, ManualClock};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_sync_enrol::{
    DirectoryConfig, DurableDirectory, EnrolRequest, EnrolService, EnrolmentStore, Ids,
    MemoryEnrolmentStore, NodeIdentity, PgEnrolmentStore, RandomSource,
};
use spot_sync_http::{ClientCertificate, NodeDirectory};
use spot_sync_hub_host::enrol::{ENROL_PATH, Redeemer, enrol_router};
use spot_sync_hub_host::{HubCore, TenantSinks};
use spot_sync_hub_pg::PgHub;
use spot_sync_protocol::{NodeId, Record};
use time::OffsetDateTime;
use tower::ServiceExt;

const T0: i64 = 1_767_225_600;
const POOL: usize = 6;

// ---------------------------------------------------------------------------------------------
// Doubles.
// ---------------------------------------------------------------------------------------------

struct StepClock(AtomicI64);

impl spot_sync_enrol::Clock for StepClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct CountingRandom(AtomicU64);

impl RandomSource for CountingRandom {
    fn fill(&self, buffer: &mut [u8]) {
        for byte in buffer {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            *byte = (n.wrapping_mul(2_654_435_761) >> 7) as u8;
        }
    }
}

#[derive(Default)]
struct SeqIds(AtomicU64);

impl Ids for SeqIds {
    fn uuid(&self) -> String {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        format!("00000000-0000-4000-8000-{n:012x}")
    }
    fn enrolment_id(&self) -> String {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        format!("enrol_{n}")
    }
}

struct NullCore;

impl HubCore for NullCore {
    fn tick(&mut self, _now_ms: i64) -> Result<(), String> {
        Ok(())
    }
    fn window(&mut self, _dest: &str, _max: usize) -> Result<Vec<Record>, String> {
        Ok(Vec::new())
    }
    fn ack_served(&mut self, _dest: &str, _through: u64, _now_ms: i64) -> Result<(), String> {
        Ok(())
    }
    fn destinations(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Assembly;

impl RelayAssembly for Assembly {
    fn metadata_epoch(&self) -> u64 {
        7
    }
    fn sinks(&self) -> Arc<TenantSinks> {
        Arc::new(TenantSinks::new())
    }
    fn core(&self, _hub: &PgHub) -> Result<Box<dyn HubCore>, String> {
        Ok(Box::new(NullCore))
    }
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn request(name: &str) -> EnrolRequest {
    EnrolRequest {
        node: node(name),
        role: "branch".to_owned(),
        partitions: vec!["branch_01".to_owned()],
        template: "{}".to_owned(),
        ttl_seconds: None,
    }
}

fn site_clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(T0).unwrap(),
    ))
}

// ---------------------------------------------------------------------------------------------
// Settings and fail-closed registration (no database).
// ---------------------------------------------------------------------------------------------

#[test]
fn the_module_is_hub_only_and_between_the_role_and_the_listener() {
    assert_eq!(sync_hub_parts::ROLES, ["hub"]);
    const { assert!(sync_hub_parts::ORDER > spot_cli::services::node_role::ORDER) };
    const { assert!(sync_hub_parts::ORDER < spot_cli::services::sync_hub::ORDER) };
}

#[test]
fn settings_default_and_every_bound_is_checked() {
    let resolved = Settings::default().resolve().unwrap();
    assert_eq!(resolved.hub_name, "hub");
    assert_eq!(resolved.refresh, Duration::from_secs(2));
    assert_eq!((resolved.enrol.per_second, resolved.enrol.queued), (5, 8));
    for bad in [
        Settings {
            directory_refresh_secs: Some(0),
            ..Settings::default()
        },
        Settings {
            directory_refresh_secs: Some(6),
            ..Settings::default()
        },
        Settings {
            enrol_per_second: Some(0),
            ..Settings::default()
        },
        Settings {
            enrol_per_second: Some(101),
            ..Settings::default()
        },
        Settings {
            enrol_queued: Some(0),
            ..Settings::default()
        },
        Settings {
            enrol_queued: Some(65),
            ..Settings::default()
        },
    ] {
        assert!(bad.resolve().is_err(), "{bad:?}");
    }
    let parsed: Result<Settings, _> = toml::from_str("hub_name = \"hub\"\nspeed = 1");
    assert!(parsed.is_err(), "an unknown key is refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_without_its_ports_refuses_to_register_and_names_what_is_missing() {
    let clock = site_clock();
    let mut ctx = SiteCtx::new("site", Duration::from_secs(1), clock, BTreeMap::new(), true);
    ctx.provide(Arc::new(ActiveRole::new(Role::Hub))).unwrap();
    let error = sync_hub_parts::register(&mut ctx).await.unwrap_err();
    assert!(error.0.contains(PART_MISSING), "{error:?}");
    assert!(error.0.contains("random source"));
    assert!(error.0.contains("relay assembly"));
    assert!(
        ctx.port::<spot_cli::services::sync_hub::HubParts>()
            .is_none()
    );
}

// ---------------------------------------------------------------------------------------------
// The enrolment flow over the in-memory store (no database).
// ---------------------------------------------------------------------------------------------

struct Rig {
    service: Arc<EnrolService>,
    directory: Arc<DurableDirectory>,
    clock: Arc<StepClock>,
}

fn memory_rig() -> Rig {
    let store: Arc<dyn EnrolmentStore> = Arc::new(MemoryEnrolmentStore::new());
    let clock = Arc::new(StepClock(AtomicI64::new(T0)));
    let directory = Arc::new(DurableDirectory::new(
        Arc::clone(&store),
        clock.clone(),
        DirectoryConfig::default(),
    ));
    directory.refresh().unwrap();
    let service = Arc::new(EnrolService::new(
        store,
        Arc::clone(&directory),
        Arc::new(CountingRandom::default()),
        Arc::new(SeqIds::default()),
        clock.clone(),
    ));
    Rig {
        service,
        directory,
        clock,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn enrol_request_body(node: &str, code: &str, certificate: &[u8]) -> String {
    serde_json::json!({ "node": node, "code": code, "certificate": hex(certificate) }).to_string()
}

async fn post_enrol(app: axum::Router, body: String) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri(ENROL_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_code_enrols_a_node_once_and_the_directory_then_authenticates_it() {
    let rig = memory_rig();
    let identity = NodeIdentity::generate_replacement(&node("edge-khi#branch_01"), T0).unwrap();
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge-khi#branch_01"))
        .unwrap();
    let app = enrol_router(
        Arc::new(EnrolRedeemer::new(Arc::clone(&rig.service))),
        Default::default(),
        site_clock(),
    )
    .unwrap();

    // Before the redemption the certificate is nobody.
    let certificate = ClientCertificate::from_der(identity.certificate_der().to_vec());
    assert!(rig.directory.resolve(&certificate).is_err());

    let body = enrol_request_body(
        "edge-khi#branch_01",
        &issued.code,
        identity.certificate_der(),
    );
    let (status, _) = post_enrol(app.clone(), body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let authenticated = rig.directory.resolve(&certificate).unwrap();
    assert_eq!(authenticated.node, node("edge-khi#branch_01"));
    assert!(authenticated.partitions.contains("branch_01"));

    // The code is single use, and a wrong one gets the same answer: nothing says which it was.
    let (status, used) = post_enrol(app.clone(), body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, wrong) = post_enrol(
        app,
        enrol_request_body(
            "edge-khi#branch_01",
            "enrol_9.00112233445566778899aabbccddeeff",
            identity.certificate_der(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(used, wrong);
    assert!(used.contains("sync_enrol.code_invalid"));
    assert!(!used.contains(&issued.code));
}

#[test]
fn a_wrong_code_and_a_lockout_map_to_their_statuses_and_leak_nothing() {
    let rig = memory_rig();
    let redeemer = EnrolRedeemer::new(Arc::clone(&rig.service));
    let identity = NodeIdentity::generate_replacement(&node("edge-khi#branch_01"), T0).unwrap();
    let mut statuses = Vec::new();
    for attempt in 0..8 {
        let outcome = redeemer.redeem(
            &format!("enrol_{attempt}.00112233445566778899aabbccddeeff"),
            "edge-khi#branch_01",
            identity.certificate_der(),
            "node:edge-khi#branch_01",
        );
        let refusal = outcome.unwrap_err();
        assert!(!refusal.message.contains("00112233445566778899aabbccddeeff"));
        statuses.push(refusal.status);
    }
    assert_eq!(statuses[0], 403);
    assert!(
        statuses.contains(&429),
        "repeated failures from one source are locked out: {statuses:?}"
    );
    let invalid = refusal_of(&spot_sync_enrol::EnrolError::CodeInvalid);
    assert_eq!(invalid.status, 403);
}

#[test]
fn a_revoked_node_is_no_longer_admitted_at_the_next_heartbeat() {
    use spot_sync_http::guard::{DirectoryGate, HeartbeatCheck};
    let rig = memory_rig();
    let identity = NodeIdentity::generate_replacement(&node("edge-khi#branch_01"), T0).unwrap();
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge-khi#branch_01"))
        .unwrap();
    rig.service
        .redeem(
            &issued.code,
            &node("edge-khi#branch_01"),
            identity.certificate_der(),
            "test",
        )
        .unwrap();
    let certificate = ClientCertificate::from_der(identity.certificate_der().to_vec());
    let check = HeartbeatCheck::new(
        Arc::new(DirectoryGate::new(
            Arc::clone(&rig.directory) as Arc<dyn NodeDirectory>
        )),
        certificate,
    );
    assert!(check.still_admitted());
    rig.service
        .revoke("admin", &node("edge-khi#branch_01"))
        .unwrap();
    assert!(
        !check.still_admitted(),
        "the open pull ends at the next heartbeat"
    );
    let _ = rig.clock.0.load(Ordering::SeqCst);
}

// ---------------------------------------------------------------------------------------------
// PostgreSQL.
// ---------------------------------------------------------------------------------------------

const OUTBOX_DDL: &str = "CREATE TABLE IF NOT EXISTS spot_outbox (\
    id uuid PRIMARY KEY, tx uuid NOT NULL, ordinal integer NOT NULL, topic text NOT NULL, \
    subject_entity text NOT NULL, subject_id uuid NOT NULL, subject_version bigint, \
    schema_version integer NOT NULL, payload text NOT NULL, captured_at timestamptz NOT NULL, \
    xid xid8 NOT NULL)";

struct PgWorld {
    rt: tokio::runtime::Runtime,
    _db: TestDb,
    config: PgConfig,
}

fn pg_world() -> Option<PgWorld> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let Some(db) = rt.block_on(TestDb::create()) else {
        assert!(
            std::env::var("SPOT_REQUIRE_PG").as_deref() != Ok("1"),
            "SPOT_REQUIRE_PG=1 but no database is configured"
        );
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return None;
    };
    let client = rt.block_on(db.raw_client());
    rt.block_on(client.batch_execute(OUTBOX_DDL)).unwrap();
    let mut config = db.config();
    config.pool_max_size = POOL;
    Some(PgWorld {
        rt,
        _db: db,
        config,
    })
}

fn backend(world: &PgWorld) -> PgBackend {
    PgBackend::connect_unchecked(world.config.clone()).expect("backend")
}

/// Creates the hub and enrolment tables for `tenant` on a plain thread.
fn prepare(world: &PgWorld, tenant: &str) {
    let backend = backend(world);
    let bridge = Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "sync_hub_parts_prepare".to_owned(),
                workers: 2,
                max_in_flight: 3,
                queue_capacity: 16,
                default_deadline: Duration::from_secs(30),
                max_deadline: Duration::from_secs(60),
            },
            &backend,
        )
        .expect("bridge"),
    );
    PgHub::new(Arc::clone(&bridge), POOL, tenant)
        .unwrap()
        .prepare()
        .unwrap();
    PgEnrolmentStore::new(bridge, POOL, tenant)
        .unwrap()
        .prepare()
        .unwrap();
}

fn inputs(world: &PgWorld, tenant: &str) -> Inputs {
    Inputs {
        backend: backend(world),
        pool_size: POOL,
        tenant: tenant.to_owned(),
        hub_node: node(&format!("{tenant}#hub")),
        resolved: Settings::default().resolve().unwrap(),
        clock: site_clock(),
        random: Arc::new(CountingRandom::default()),
        ids: Arc::new(SeqIds::default()),
        assembly: Arc::new(Assembly),
    }
}

#[test]
fn the_parts_build_enrol_and_survive_a_restart() {
    let Some(world) = pg_world() else { return };
    prepare(&world, "acme");
    let identity = NodeIdentity::generate_replacement(&node("acme#branch_01"), T0).unwrap();
    let certificate = ClientCertificate::from_der(identity.certificate_der().to_vec());

    let built = sync_hub_parts::build(inputs(&world, "acme")).unwrap();
    assert_eq!(built.parts.metadata_epoch, 7);
    assert!(!built.directory.needs_refresh(), "loaded before serving");
    assert!(built.directory.resolve(&certificate).is_err());

    let issued = built
        .service
        .create_enrolment("admin", &request("acme#branch_01"))
        .unwrap();
    let (status, _) = world.rt.block_on(post_enrol(
        built.enrol.clone(),
        enrol_request_body("acme#branch_01", &issued.code, identity.certificate_der()),
    ));
    assert_eq!(status, StatusCode::OK);
    assert!(built.directory.resolve(&certificate).is_ok());

    // A new process over the same database: the directory loads the node again.
    let again = sync_hub_parts::build(inputs(&world, "acme")).unwrap();
    assert!(again.directory.resolve(&certificate).is_ok());

    // Another tenant never sees it.
    prepare(&world, "other");
    let other = sync_hub_parts::build(inputs(&world, "other")).unwrap();
    assert!(other.directory.resolve(&certificate).is_err());
}

#[test]
fn a_missing_table_is_a_named_start_up_failure_not_a_default() {
    let Some(world) = pg_world() else { return };
    // No `prepare`: the hub tables do not exist.
    let error = match sync_hub_parts::build(inputs(&world, "acme")) {
        Ok(_) => panic!("a hub without its tables must not build"),
        Err(error) => error,
    };
    assert!(
        error.0.contains("sync_hub_parts.part_unavailable"),
        "{error:?}"
    );
    assert!(error.0.contains("spot migrate"));
}
