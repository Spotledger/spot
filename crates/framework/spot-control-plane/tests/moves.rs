//! The `moves` module: a tenant move between two clusters over fake host ports and the in-memory
//! registry and move store. Covers the freeze and cut-over order (a fault-injection timeline), a
//! kill at every step and the resume, cancel before and after the cut-over, the refusals, the
//! cross-tenant package and the scope. The PostgreSQL row runs through `SPOT_TEST_PG_URL` and
//! prints why it skips when that is unset.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_control_plane::auth::{KeyVault, sha256_hex};
use spot_control_plane::config::Config;
use spot_control_plane::modules::moves::{
    self, BUSY_MIGRATION, BUSY_RESTORE, MoveDeps, MovesState, SourceQuarantine, TenantBusy,
};
use spot_control_plane::modules::tenants::{Kv, MemoryKv};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::ports::{AuditSink, BoxFuture as MgmtFuture, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_rehearsal::{Dimension, Fingerprint, FingerprintRow, FingerprintSection};
use spot_signed_command::{Command, MemoryNonceStore, Signer, SoftwareSigner, sign};
use spot_tenancy::{
    Cluster, ClusterId, ControlStore, DatabaseId, Edition, MemoryStore, Placement, Region, Tenant,
    TenantFlags, TenantId, TenantName, TenantState,
};
use spot_tenant_move::{
    BoxFuture, Chunk, ControlStorePlacement, Error as MoveError, FingerprintSource,
    MemoryMoveStore, MoveMode, MovePlan, PlacementPort, QuiescePort, Result as MoveResult,
    RoutingCacheInvalidator, Side, TenancyResidency, TransferPort,
};
use std::collections::BTreeMap;
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;
const DAY_MS: u64 = 86_400_000;

struct MemVault(String);
impl KeyVault for MemVault {
    fn ca_key_pem(&self) -> Result<String, spot_control_plane::auth::CaError> {
        Ok(self.0.clone())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn config_text(admin_key_hex: &str) -> String {
    format!(
        r#"
region = "eu-1"
audience = "cp-eu-1"
administrator_keys = ["{admin_key_hex}"]
[database]
host = "127.0.0.1"
dbname = "control"
user = "cp"
password_env = "SPOT_CP_TEST_PASSWORD_UNSET"
tls = false
[operator_listener]
listen = "127.0.0.1:0"
server_cert = "/nonexistent/op.pem"
server_key = "/nonexistent/op.key"
client_ca = "/nonexistent/operator-ca.pem"
[host_listener]
listen = "127.0.0.1:1"
server_cert = "/nonexistent/host.pem"
server_key = "/nonexistent/host.key"
client_ca = "/nonexistent/host-ca.pem"
[host_ca]
cert = "/nonexistent/host-ca.pem"
key = "/nonexistent/host-ca.key"
common_name = "Spot host CA"
[[operators]]
subject = "CN=root"
principal = "root"
tenants = ["*"]
[[operators]]
subject = "CN=alice"
principal = "alice"
tenants = ["00000000-0000-7000-8000-000000000000"]
"#
    )
}

// ---------------------------------------------------------------------------------------------
// Fake host ports: one log of what the source and the target saw, in order
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Hosts {
    events: Mutex<Vec<String>>,
    frozen: AtomicBool,
    fail_once: Mutex<Option<String>>,
    busy: Mutex<Option<&'static str>>,
    mismatch: AtomicBool,
    manifest_tenant: Mutex<Option<String>>,
    violations: Mutex<Vec<String>>,
}

impl Hosts {
    /// Logs the step, then fails it once (after its effect) when asked to.
    fn step(&self, plan: &MovePlan, name: &str) -> MoveResult<()> {
        self.events.lock().unwrap().push(name.to_owned());
        let mut slot = self.fail_once.lock().unwrap();
        if slot.as_deref() == Some(name) {
            *slot = None;
            return Err(MoveError::TransferFailed {
                tenant: plan.tenant.to_string(),
                reason: "fake.killed",
            });
        }
        Ok(())
    }

    fn violation(&self, text: &str) {
        self.violations.lock().unwrap().push(text.to_owned());
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.events().iter().position(|e| e == name)
    }

    fn count(&self, name: &str) -> usize {
        self.events().iter().filter(|e| *e == name).count()
    }

    fn chunks(&self, plan: &MovePlan) -> Vec<Chunk> {
        let named = self
            .manifest_tenant
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| plan.tenant.to_string());
        let manifest = format!("tenant = \"{named}\"\n").into_bytes();
        let bodies: [(&str, Vec<u8>); 3] = [
            ("snapshot.toml", manifest),
            ("dump.bin", b"abc".to_vec()),
            ("dump.bin", b"de".to_vec()),
        ];
        let mut offset = 0u64;
        let mut out = Vec::new();
        for (index, (file, bytes)) in bodies.into_iter().enumerate() {
            let length = u64::try_from(bytes.len()).unwrap();
            out.push(Chunk::new(
                u64::try_from(index).unwrap(),
                file,
                offset,
                bytes,
                index == 2,
            ));
            offset += length;
        }
        out
    }
}

impl QuiescePort for Hosts {
    fn quiesce<'a>(
        &'a self,
        plan: &'a MovePlan,
        _move_id: &'a spot_tenant_move::MoveId,
        _now: u64,
    ) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async move {
            self.frozen.store(true, Ordering::SeqCst);
            self.step(plan, "freeze")
        })
    }

    fn resume<'a>(
        &'a self,
        plan: &'a MovePlan,
        _move_id: &'a spot_tenant_move::MoveId,
        _now: u64,
    ) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async move {
            self.frozen.store(false, Ordering::SeqCst);
            self.step(plan, "resume")
        })
    }
}

impl TransferPort for Hosts {
    fn export_chunk<'a>(
        &'a self,
        plan: &'a MovePlan,
        after: Option<u64>,
    ) -> BoxFuture<'a, MoveResult<Option<Chunk>>> {
        Box::pin(async move {
            let index = after.map_or(0, |a| a + 1);
            let chunks = self.chunks(plan);
            let Some(chunk) = chunks.get(usize::try_from(index).unwrap()) else {
                return Ok(None);
            };
            if !self.frozen.load(Ordering::SeqCst) {
                self.violation("export while the source accepts writes");
            }
            self.step(plan, &format!("export:{index}"))?;
            Ok(Some(chunk.clone()))
        })
    }

    fn import_chunk<'a>(
        &'a self,
        plan: &'a MovePlan,
        chunk: &'a Chunk,
    ) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async move { self.step(plan, &format!("import:{}", chunk.sequence)) })
    }

    fn finish_import<'a>(&'a self, plan: &'a MovePlan) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async move { self.step(plan, "finish_import") })
    }

    fn cleanup<'a>(&'a self, plan: &'a MovePlan) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async move { self.step(plan, "cleanup") })
    }
}

fn fingerprint(count: i128) -> Fingerprint {
    Fingerprint::new(vec![FingerprintSection::new(
        "documents",
        "Documents",
        vec!["doctype".to_owned()],
        vec!["count".to_owned()],
        vec![FingerprintRow::new(
            vec![Dimension::new("doctype", "invoice")],
            BTreeMap::from([("count".to_owned(), count)]),
        )],
    )])
}

impl FingerprintSource for Hosts {
    fn measure<'a>(
        &'a self,
        plan: &'a MovePlan,
        side: Side,
    ) -> BoxFuture<'a, MoveResult<Fingerprint>> {
        Box::pin(async move {
            let name = match side {
                Side::Source => "fp:source",
                Side::Target => "fp:target",
            };
            self.step(plan, name)?;
            let skew = i128::from(side == Side::Target && self.mismatch.load(Ordering::SeqCst));
            Ok(fingerprint(10 + skew))
        })
    }
}

impl TenantBusy for Hosts {
    fn busy<'a>(
        &'a self,
        _tenant: &'a TenantId,
    ) -> MgmtFuture<'a, Result<Option<&'static str>, CpError>> {
        Box::pin(async move { Ok(*self.busy.lock().unwrap()) })
    }
}

impl SourceQuarantine for Hosts {
    fn keep_read_only<'a>(
        &'a self,
        plan: &'a MovePlan,
        _move_id: &'a spot_tenant_move::MoveId,
        _until_ms: u64,
    ) -> MgmtFuture<'a, Result<(), &'static str>> {
        Box::pin(async move {
            if !self.frozen.load(Ordering::SeqCst) {
                self.violation("the source was reopened before the quarantine");
            }
            self.step(plan, "quarantine").map_err(|_| "fake.killed")
        })
    }
}

struct NoCache;
impl RoutingCacheInvalidator for NoCache {
    fn invalidate(&self, _tenant: &TenantId) -> Result<(), &'static str> {
        Ok(())
    }
}

/// The real placement flip over the registry, logged, with the freeze asserted at the flip and an
/// optional kill right after it landed (the retry must find the flip done).
struct LoggedPlacement {
    inner: ControlStorePlacement<MemoryStore>,
    hosts: Arc<Hosts>,
}

impl PlacementPort for LoggedPlacement {
    fn cut_over<'a>(&'a self, plan: &'a MovePlan, now: u64) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async move {
            if !self.hosts.frozen.load(Ordering::SeqCst) {
                self.hosts
                    .violation("cut-over while the source accepts writes");
            }
            let verified = self.hosts.position("fp:target").is_some();
            if !verified {
                self.hosts
                    .violation("cut-over before the fingerprints were compared");
            }
            self.inner.cut_over(plan, now).await?;
            self.hosts.step(plan, "cut_over")
        })
    }

    fn restore<'a>(&'a self, plan: &'a MovePlan) -> BoxFuture<'a, MoveResult<()>> {
        self.inner.restore(plan)
    }
}

// ---------------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------------

thread_local! {
    static STATE: RefCell<Option<Arc<MovesState>>> = const { RefCell::new(None) };
}

fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = STATE.with(|s| s.borrow().clone()).expect("state set");
    moves::register_with(ctx, state);
    Ok(())
}

struct Fixture {
    plane: ControlPlane,
    state: Arc<MovesState>,
    hosts: Arc<Hosts>,
    store: Arc<MemoryStore>,
    clock: Arc<ManualClock>,
    signer: SoftwareSigner,
    kv: Arc<dyn Kv>,
}

fn cluster(name: &str, db: &str, region: &str, accepting: bool) -> Cluster {
    Cluster {
        cluster: ClusterId::new(name).unwrap(),
        database: DatabaseId::new(db).unwrap(),
        region: Region::new(region).unwrap(),
        capacity: 10,
        accepting,
    }
}

/// An active tenant at `c1/d1` in region `eu-1`.
fn seed_tenant(store: &MemoryStore, seed: u8) -> TenantId {
    let id = TenantId::new_v7(1_700_000_000_000, [seed; 10]);
    let tenant = Tenant {
        id,
        name: TenantName::new(format!("tenant-{seed}")).unwrap(),
        region: Region::new("eu-1").unwrap(),
        edition: Edition::new("standard").unwrap(),
        state: TenantState::Provisioning,
        placement: Placement {
            cluster: ClusterId::new("c1").unwrap(),
            database: DatabaseId::new("d1").unwrap(),
            schema: id.schema_name(),
        },
        created: 1_700_000_000_000,
        flags: TenantFlags::NONE,
        version: 0,
        retention: None,
        name_reserved_until: None,
    };
    let mut stored = store.insert_within(tenant, 1_700_000_000_000, 10).unwrap();
    stored.state = TenantState::Active;
    let version = stored.version;
    store.update(stored, version).unwrap();
    id
}

fn fixture_with(kv: Arc<dyn Kv>, tenants: &[u8]) -> (Fixture, Vec<TenantId>) {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let hosts = Arc::new(Hosts::default());
    let store = Arc::new(MemoryStore::new());
    let ids: Vec<TenantId> = tenants
        .iter()
        .map(|seed| seed_tenant(&store, *seed))
        .collect();
    let deps = MoveDeps {
        tenants: store.clone(),
        moves: Arc::new(MemoryMoveStore::new()),
        quiesce: hosts.clone(),
        transfer: hosts.clone(),
        placement: Arc::new(LoggedPlacement {
            inner: ControlStorePlacement::new(Arc::clone(&store), Arc::new(NoCache)),
            hosts: Arc::clone(&hosts),
        }),
        residency: Arc::new(TenancyResidency::new(Arc::clone(&store))),
        fingerprints: hosts.clone(),
        busy: hosts.clone(),
        quarantine: hosts.clone(),
        kv: Arc::clone(&kv),
        clusters: vec![
            cluster("c1", "d1", "eu-1", true),
            cluster("c2", "d2", "eu-1", true),
            cluster("c3", "d3", "eu-1", false),
            cluster("c4", "d4", "us-1", true),
        ],
        mode: MoveMode::Offline,
        quarantine_ms: 7 * DAY_MS,
        poll_ms: 10,
        max_attempts: 5,
    };
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let state = MovesState::ready(clock_dyn.clone(), deps);
    STATE.with(|s| *s.borrow_mut() = Some(Arc::clone(&state)));
    let ca = KeyPair::generate().unwrap().serialize_pem();
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: None,
        clock: clock_dyn,
        audit,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(ca)),
        registrations: vec![Registration {
            name: "moves",
            order: moves::ORDER,
            register,
        }],
    })
    .unwrap();
    (
        Fixture {
            plane,
            state,
            hosts,
            store,
            clock,
            signer,
            kv,
        },
        ids,
    )
}

fn fixture(tenants: &[u8]) -> (Fixture, Vec<TenantId>) {
    fixture_with(Arc::new(MemoryKv::new()), tenants)
}

impl Fixture {
    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn header(&self, path: &str, body: &str) -> String {
        let command = Command::new(
            "cp.test",
            json!({ "body_sha256": sha256_hex(body.as_bytes()), "path": path }),
            "cp-eu-1",
            self.signer.key_id(),
            self.now() - 5,
            self.now() + 300,
        );
        let signed = sign(&command, &self.signer).unwrap();
        let text = spot_mgmt_api::wire::encode(&signed, &[]).unwrap();
        URL_SAFE_NO_PAD.encode(text.as_bytes())
    }

    async fn send(
        &self,
        who: &str,
        method: &str,
        path: &str,
        body: &str,
        key: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder =
            Request::builder()
                .method(method)
                .uri(path)
                .extension(PeerCertificate::new(
                    &format!("CN={who}"),
                    &format!("fp-{who}"),
                ));
        if method == "POST" {
            builder = builder.header("x-spot-command", self.header(path, body));
        }
        if let Some(key) = key {
            builder = builder.header("idempotency-key", key);
        }
        let response = self
            .plane
            .operator_router
            .clone()
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn start_as(
        &self,
        who: &str,
        tenant: &TenantId,
        to: &str,
        key: &str,
    ) -> (StatusCode, Value) {
        let body = json!({ "tenant_id": tenant.to_string(), "to_cluster": to }).to_string();
        self.send(who, "POST", "/cp/v1/moves", &body, Some(key))
            .await
    }

    async fn start(&self, tenant: &TenantId, to: &str, key: &str) -> (StatusCode, Value) {
        self.start_as("root", tenant, to, key).await
    }

    async fn state_of(&self, tenant: &TenantId) -> String {
        let (status, body) = self
            .send(
                "root",
                "GET",
                &format!("/cp/v1/moves/mv-{tenant}"),
                "",
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"]["state"].as_str().unwrap().to_owned()
    }

    fn placement_of(&self, tenant: &TenantId) -> String {
        self.store
            .get(tenant)
            .unwrap()
            .placement
            .cluster
            .as_str()
            .to_owned()
    }

    /// Runs the runner until the move settles (at most `passes`).
    async fn settle(&self, tenant: &TenantId, passes: usize) -> String {
        for _ in 0..passes {
            let driven = moves::run_once(&self.state).await;
            let state = self.state_of(tenant).await;
            // A failed move is aborted by the next pass: wait until nothing is left to drive.
            if matches!(state.as_str(), "done" | "cancelled") || (state == "failed" && driven == 0)
            {
                return state;
            }
        }
        self.state_of(tenant).await
    }
}

fn error_code(body: &Value) -> Option<&str> {
    body["error"]["code"].as_str()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_move_freezes_first_commits_once_and_keeps_the_source_frozen_afterwards() {
    let (f, ids) = fixture(&[1]);
    let tenant = ids[0];
    let (status, started) = f.start(&tenant, "c2", "k1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["data"]["state"], "requested");
    assert_eq!(started["data"]["id"], format!("mv-{tenant}"));
    assert_eq!(started["data"]["to_cluster"], "c2");
    assert!(f.hosts.events().is_empty(), "the request freezes nothing");
    assert_eq!(f.placement_of(&tenant), "c1");

    assert_eq!(f.settle(&tenant, 3).await, "done");
    assert_eq!(f.placement_of(&tenant), "c2");
    let events = f.hosts.events();
    let at = |name: &str| {
        f.hosts
            .position(name)
            .unwrap_or_else(|| panic!("{name} missing: {events:?}"))
    };
    assert!(at("freeze") < at("export:0"), "{events:?}");
    assert!(at("export:2") < at("import:0"), "{events:?}");
    assert!(at("finish_import") < at("fp:source"), "{events:?}");
    assert!(at("fp:target") < at("cut_over"), "{events:?}");
    assert!(at("cut_over") < at("quarantine"), "{events:?}");
    assert_eq!(f.hosts.count("cut_over"), 1);
    assert_eq!(
        f.hosts.count("resume"),
        0,
        "the source is never reopened: {events:?}"
    );
    assert!(
        f.hosts.frozen.load(Ordering::SeqCst),
        "the source stays frozen"
    );
    assert_eq!(
        f.hosts.violations.lock().unwrap().len(),
        0,
        "{:?}",
        f.hosts.violations
    );
    // The end of the quarantine is recorded for the retention task.
    let until =
        f.kv.get("move_quarantine", &tenant.to_string())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        until.parse::<u64>().unwrap(),
        u64::try_from(NOW).unwrap() * 1000 + 7 * DAY_MS
    );
    // Nothing is left for the runner.
    assert_eq!(moves::run_once(&f.state).await, 0);
}

#[tokio::test]
async fn the_idempotency_key_replays_and_a_changed_body_is_replayed_code() {
    let (f, ids) = fixture(&[1]);
    let (status, first) = f.start(&ids[0], "c2", "k1").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, again) = f.start(&ids[0], "c2", "k1").await;
    assert_eq!((status, again), (StatusCode::ACCEPTED, first));
    let (status, other) = f.start(&ids[0], "c3", "k1").await;
    assert_eq!(
        (status, error_code(&other)),
        (StatusCode::CONFLICT, Some("cp.replayed"))
    );
    // A missing key is refused.
    let body = json!({ "tenant_id": ids[0].to_string(), "to_cluster": "c2" }).to_string();
    let (status, _) = f.send("root", "POST", "/cp/v1/moves", &body, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // The same tenant cannot be moved twice at once under another key.
    let (status, second) = f.start(&ids[0], "c2", "k2").await;
    assert_eq!(
        (status, error_code(&second)),
        (StatusCode::CONFLICT, Some("cp.conflict"))
    );
}

#[tokio::test]
async fn a_request_is_refused_before_anything_is_frozen() {
    let (f, ids) = fixture(&[1, 2, 3]);
    // A migration or a restore running on the tenant's host.
    *f.hosts.busy.lock().unwrap() = Some(BUSY_MIGRATION);
    let (status, body) = f.start(&ids[0], "c2", "a").await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, Some("cp.conflict"))
    );
    *f.hosts.busy.lock().unwrap() = Some(BUSY_RESTORE);
    let (status, _) = f.start(&ids[0], "c2", "b").await;
    assert_eq!(status, StatusCode::CONFLICT);
    *f.hosts.busy.lock().unwrap() = None;
    // An unknown target, the tenant's own cluster, a target that takes no tenants.
    let (status, body) = f.start(&ids[0], "nowhere", "c").await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("cp.invalid"))
    );
    let (status, _) = f.start(&ids[0], "c1", "d").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = f.start(&ids[0], "c3", "e").await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Residency: the target is in another region.
    let (status, body) = f.start(&ids[1], "c4", "f").await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::FORBIDDEN, Some("cp.forbidden"))
    );
    // A tenant that is not active.
    let mut leaving = f.store.get(&ids[2]).unwrap();
    leaving.state = TenantState::Deprovisioning;
    let version = leaving.version;
    f.store.update(leaving, version).unwrap();
    let (status, body) = f.start(&ids[2], "c2", "g").await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, Some("cp.conflict"))
    );
    // Every refusal left the hosts untouched and every key free for the corrected request.
    assert!(f.hosts.events().is_empty());
    let (status, _) = f.start(&ids[0], "c2", "a").await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn scope_is_not_an_oracle() {
    let (f, ids) = fixture(&[1]);
    let (status, started) = f.start(&ids[0], "c2", "k1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    // alice may not see the tenant: the same answer as for an id that does not exist.
    let (status, hidden) = f.start_as("alice", &ids[0], "c2", "k9").await;
    assert_eq!(
        (status, error_code(&hidden)),
        (StatusCode::NOT_FOUND, Some("cp.not_found"))
    );
    let path = format!("/cp/v1/moves/mv-{}", ids[0]);
    let (status, body) = f.send("alice", "GET", &path, "", None).await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::NOT_FOUND, Some("cp.not_found"))
    );
    let (status, missing) = f
        .send("alice", "GET", "/cp/v1/moves/mv-not-an-id", "", None)
        .await;
    assert_eq!(
        (status, error_code(&missing)),
        (StatusCode::NOT_FOUND, error_code(&body))
    );
    let (status, _) = f
        .send("alice", "POST", &format!("{path}/cancel"), "", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_kill_at_each_step_resumes_to_the_same_end_state() {
    for step in [
        "freeze",
        "export:1",
        "import:1",
        "finish_import",
        "fp:source",
        "fp:target",
        "cut_over",
        "quarantine",
    ] {
        let (f, ids) = fixture(&[1]);
        let tenant = ids[0];
        *f.hosts.fail_once.lock().unwrap() = Some(step.to_owned());
        let (status, _) = f.start(&tenant, "c2", "k1").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        // The first pass dies at the step, after its effect landed; it is not "done" and not "failed".
        moves::run_once(&f.state).await;
        let midway = f.state_of(&tenant).await;
        assert!(
            !matches!(midway.as_str(), "done" | "failed" | "cancelled"),
            "{step}: {midway}"
        );
        assert_eq!(f.settle(&tenant, 3).await, "done", "{step}");
        assert_eq!(f.placement_of(&tenant), "c2", "{step}");
        assert_eq!(
            f.hosts.count("cut_over"),
            if step == "cut_over" { 2 } else { 1 },
            "{step}"
        );
        assert_eq!(
            f.hosts.count("resume"),
            0,
            "{step}: the source was reopened"
        );
        assert!(
            f.hosts.violations.lock().unwrap().is_empty(),
            "{step}: {:?}",
            f.hosts.violations
        );
    }
}

#[tokio::test]
async fn a_failure_before_the_cut_over_aborts_and_the_source_serves_again() {
    let (f, ids) = fixture(&[1]);
    let tenant = ids[0];
    f.hosts.mismatch.store(true, Ordering::SeqCst);
    f.start(&tenant, "c2", "k1").await;
    // The mismatch cannot be retried away: the first pass records it, the next aborts.
    assert_eq!(f.settle(&tenant, 4).await, "failed");
    assert_eq!(f.placement_of(&tenant), "c1", "the placement never flipped");
    assert_eq!(f.hosts.count("cut_over"), 0);
    assert_eq!(f.hosts.count("resume"), 1, "the source was resumed");
    assert!(!f.hosts.frozen.load(Ordering::SeqCst));
    assert_eq!(f.store.get(&tenant).unwrap().state, TenantState::Active);
}

#[tokio::test]
async fn cancel_before_the_cut_over_restores_service_and_after_it_is_a_conflict() {
    let (f, ids) = fixture(&[1, 2]);
    // Cancelled while still queued: nothing was frozen, the source is "resumed" idempotently.
    f.start(&ids[0], "c2", "k1").await;
    let path = format!("/cp/v1/moves/mv-{}/cancel", ids[0]);
    let (status, body) = f.send("root", "POST", &path, "", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["state"], "cancelled");
    assert_eq!(f.placement_of(&ids[0]), "c1");
    assert!(!f.hosts.frozen.load(Ordering::SeqCst));
    assert_eq!(f.hosts.count("freeze"), 0);
    assert_eq!(
        moves::run_once(&f.state).await,
        0,
        "a cancelled move is not driven again"
    );
    // Cancelled after the dump was taken (the source is frozen): service resumes.
    f.hosts.mismatch.store(true, Ordering::SeqCst);
    f.start(&ids[1], "c2", "k2").await;
    moves::run_once(&f.state).await;
    assert!(
        f.hosts.frozen.load(Ordering::SeqCst),
        "the stuck move holds the source frozen"
    );
    let (status, body) = f
        .send(
            "root",
            "POST",
            &format!("/cp/v1/moves/mv-{}/cancel", ids[1]),
            "",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !f.hosts.frozen.load(Ordering::SeqCst),
        "cancel reopened the source"
    );
    assert_eq!(f.placement_of(&ids[1]), "c1");
    // After the cut-over: refused.
    let (g, gids) = fixture(&[1]);
    g.start(&gids[0], "c2", "k1").await;
    assert_eq!(g.settle(&gids[0], 3).await, "done");
    let (status, body) = g
        .send(
            "root",
            "POST",
            &format!("/cp/v1/moves/mv-{}/cancel", gids[0]),
            "",
            None,
        )
        .await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, Some("cp.conflict"))
    );
    assert_eq!(g.placement_of(&gids[0]), "c2");
}

#[tokio::test]
async fn a_package_naming_another_tenant_is_refused_before_anything_is_restored() {
    let (f, ids) = fixture(&[1]);
    let tenant = ids[0];
    *f.hosts.manifest_tenant.lock().unwrap() =
        Some("00000000-0000-7000-8000-00000000dead".to_owned());
    f.start(&tenant, "c2", "k1").await;
    assert_eq!(f.settle(&tenant, 12).await, "failed");
    assert_eq!(
        f.hosts.count("finish_import"),
        0,
        "the package was never restored"
    );
    assert_eq!(f.hosts.count("cut_over"), 0);
    assert_eq!(f.placement_of(&tenant), "c1");
    assert!(!f.hosts.frozen.load(Ordering::SeqCst), "the source resumed");
}

#[tokio::test]
async fn the_module_without_host_ports_is_not_ready_and_answers_not_configured() {
    // No factory is registered in this test binary: the production wiring refuses to start.
    let ca = KeyPair::generate().unwrap().serialize_pem();
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: None,
        clock,
        audit: Arc::new(MemoryAuditSink::new()),
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(ca)),
        registrations: vec![Registration {
            name: "moves",
            order: moves::ORDER,
            register: moves::register,
        }],
    })
    .unwrap();
    // `register` without SPOT_CP_TENANTS_CONFIG mounts nothing and reports not ready.
    let response = plane
        .operator_router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/cp/v1/readiness")
                .extension(PeerCertificate::new("CN=root", "fp-root"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn postgres_rows_carry_the_same_move() {
    let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let config = spot_postgres::PgConfig::from_url(&url).unwrap();
    let pool = spot_postgres::PgBackend::connect(config)
        .await
        .unwrap()
        .pool();
    let client = pool.get().await.unwrap();
    for statement in [
        "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
        "CREATE TABLE IF NOT EXISTS cp_tenants_lease (id TEXT PRIMARY KEY, holder TEXT NOT NULL, until BIGINT NOT NULL)",
        "DELETE FROM cp_tenants_kv WHERE ns LIKE 'move_%'",
        "DELETE FROM cp_tenants_lease WHERE id LIKE 'move:%'",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    drop(client);
    let kv: Arc<dyn Kv> = Arc::new(spot_control_plane::modules::tenants::PgKv::new(pool));
    let (f, ids) = fixture_with(kv, &[1]);
    let (status, _) = f.start(&ids[0], "c2", "pg-k1").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(f.settle(&ids[0], 3).await, "done");
    assert_eq!(f.placement_of(&ids[0]), "c2");
}
