//! The `rollouts` module: canary waves, the health gate, automatic pause and explicit resume,
//! restart, the migration epoch and scope, over fake tenant hosts, a fake health gate, the
//! in-memory registry and key-value store and the real operator router. The PostgreSQL row runs
//! through `SPOT_TEST_PG_URL` and prints why it skips when that is unset.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_control_plane::auth::{KeyVault, sha256_hex};
use spot_control_plane::config::Config;
use spot_control_plane::modules::rollouts::{
    self, HealthGate, RolloutDeps, RolloutsState, StepReport, TenantMigrator,
};
use spot_control_plane::modules::tenants::{Kv, MemoryKv};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_fleet_rollout::{EdgeEpoch, Epoch, Health, SimConfig, SkewLimit, may_sync};
use spot_mgmt_api::ports::{AuditSink, BoxFuture, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::{Command, MemoryNonceStore, Signer, SoftwareSigner, sign};
use spot_tenancy::{
    ClusterId, ControlStore, DatabaseId, Edition, MemoryStore, Placement, Region, Tenant,
    TenantFlags, TenantId, TenantName, TenantState,
};
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;

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

/// The fake health surface: a colour per tenant (green by default) that can be made unreadable.
#[derive(Default)]
struct FakeHealth {
    colours: Mutex<BTreeMap<String, Health>>,
    unreadable: AtomicBool,
}

impl HealthGate for FakeHealth {
    fn colours<'a>(
        &'a self,
        tenants: &'a [String],
    ) -> BoxFuture<'a, Result<BTreeMap<String, Health>, CpError>> {
        Box::pin(async move {
            if self.unreadable.load(Ordering::SeqCst) {
                return Err(CpError::new(spot_control_plane::CpCode::Internal));
            }
            let map = self.colours.lock().unwrap();
            Ok(tenants
                .iter()
                .map(|t| (t.clone(), map.get(t).copied().unwrap_or(Health::Green)))
                .collect())
        })
    }
}

/// The fake tenant hosts: records every step in order, fails chosen steps until told to stop,
/// can turn a tenant red when its migration applies, and counts tenants in flight.
struct FakeHosts {
    health: Arc<FakeHealth>,
    calls: Mutex<Vec<(String, &'static str)>>,
    failing: Mutex<BTreeSet<(String, &'static str)>>,
    turns_red: Mutex<BTreeSet<String>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl FakeHosts {
    fn calls_of(&self, tenant: &str, step: &str) -> usize {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .filter(|(t, s)| t == tenant && *s == step)
            .count()
    }

    fn first_call(&self, tenant: &str) -> Option<usize> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .position(|(t, _)| t == tenant)
    }

    fn fail(&self, tenant: &str, step: &'static str) {
        self.failing
            .lock()
            .unwrap()
            .insert((tenant.to_owned(), step));
    }

    fn mend(&self) {
        self.failing.lock().unwrap().clear();
    }
}

impl TenantMigrator for FakeHosts {
    fn run_step<'a>(
        &'a self,
        tenant: &'a str,
        _release: &'a str,
        epoch: u64,
        step: &'static str,
    ) -> BoxFuture<'a, Result<StepReport, String>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push((tenant.to_owned(), step));
            if step == "plan" {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            }
            tokio::task::yield_now().await;
            let failed = self
                .failing
                .lock()
                .unwrap()
                .contains(&(tenant.to_owned(), step));
            if step == "apply" || failed {
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            if failed {
                return Err(format!("{step} failed on the host"));
            }
            if step == "apply" && self.turns_red.lock().unwrap().contains(tenant) {
                self.health
                    .colours
                    .lock()
                    .unwrap()
                    .insert(tenant.to_owned(), Health::Red);
            }
            Ok(StepReport {
                verified: step == "apply",
                epoch,
                rescue: Some(format!("spot migrate rescue {tenant}")),
            })
        })
    }
}

thread_local! {
    static STATE: RefCell<Option<Arc<RolloutsState>>> = const { RefCell::new(None) };
}

fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = STATE.with(|s| s.borrow().clone()).expect("state set");
    rollouts::register_with(ctx, state);
    Ok(())
}

struct Fixture {
    plane: ControlPlane,
    state: Arc<RolloutsState>,
    hosts: Arc<FakeHosts>,
    health: Arc<FakeHealth>,
    store: Arc<MemoryStore>,
    kv: Arc<dyn Kv>,
    clock: Arc<ManualClock>,
    signer: SoftwareSigner,
    tenants: Vec<String>,
    concurrency: usize,
}

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
    let mut stored = store.insert_within(tenant, 1_700_000_000_000, 100).unwrap();
    stored.state = TenantState::Active;
    let version = stored.version;
    store.update(stored, version).unwrap();
    id
}

impl Fixture {
    /// Builds the plane over fresh or shared parts (a restart reuses the parts).
    fn over(
        kv: Arc<dyn Kv>,
        store: Arc<MemoryStore>,
        hosts: Arc<FakeHosts>,
        health: Arc<FakeHealth>,
        clock: Arc<ManualClock>,
        tenants: Vec<String>,
        concurrency: usize,
    ) -> Self {
        let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
        let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
        let deps = RolloutDeps {
            tenants: store.clone(),
            health: health.clone(),
            migrator: hosts.clone(),
            kv: Arc::clone(&kv),
            soak_secs: 60,
            skew_limit: 1,
            concurrency,
            poll_ms: 10,
        };
        let clock_dyn: Arc<dyn Clock> = clock.clone();
        let state = RolloutsState::ready(clock_dyn.clone(), deps);
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
                name: "rollouts",
                order: rollouts::ORDER,
                register,
            }],
        })
        .unwrap();
        Self {
            plane,
            state,
            hosts,
            health,
            store,
            kv,
            clock,
            signer,
            tenants,
            concurrency,
        }
    }

    fn new(concurrency: usize, kv: Arc<dyn Kv>) -> Self {
        let clock = Arc::new(ManualClock::new(
            time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
        ));
        let store = Arc::new(MemoryStore::new());
        let tenants: Vec<String> = (1..=4)
            .map(|s| seed_tenant(&store, s).to_string())
            .collect();
        let health = Arc::new(FakeHealth::default());
        let hosts = Arc::new(FakeHosts {
            health: Arc::clone(&health),
            calls: Mutex::new(Vec::new()),
            failing: Mutex::new(BTreeSet::new()),
            turns_red: Mutex::new(BTreeSet::new()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        });
        Self::over(kv, store, hosts, health, clock, tenants, concurrency)
    }

    /// A new process over the same stores: the in-memory machines are gone.
    fn restart(&self) -> Self {
        Self::over(
            Arc::clone(&self.kv),
            Arc::clone(&self.store),
            Arc::clone(&self.hosts),
            Arc::clone(&self.health),
            Arc::clone(&self.clock),
            self.tenants.clone(),
            self.concurrency,
        )
    }

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

    /// Waves of tenant indexes into a create body.
    fn body(&self, waves: &[&[usize]]) -> String {
        let waves: Vec<Value> = waves
            .iter()
            .map(|w| json!({ "tenants": w.iter().map(|i| self.tenants[*i].clone()).collect::<Vec<_>>() }))
            .collect();
        json!({ "target_version": "2026.11.1", "waves": waves }).to_string()
    }

    async fn create(&self, waves: &[&[usize]], key: &str) -> (StatusCode, Value) {
        self.send(
            "root",
            "POST",
            "/cp/v1/rollouts",
            &self.body(waves),
            Some(key),
        )
        .await
    }

    async fn get(&self, id: &str) -> Value {
        let (status, body) = self
            .send("root", "GET", &format!("/cp/v1/rollouts/{id}"), "", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].clone()
    }

    async fn act(&self, id: &str, verb: &str) -> (StatusCode, Value) {
        self.send(
            "root",
            "POST",
            &format!("/cp/v1/rollouts/{id}/{verb}"),
            "",
            None,
        )
        .await
    }

    /// One runner pass, then the observation window passes.
    async fn pass(&self) {
        rollouts::run_once(&self.state).await;
        self.clock.advance(Duration::from_secs(61));
    }

    async fn passes(&self, n: usize) {
        for _ in 0..n {
            self.pass().await;
        }
    }

    /// The stored record (outcomes and rescue pointers live here).
    async fn record(&self, id: &str) -> Value {
        let text = self.kv.get("rollout", id).await.unwrap().unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

fn plain() -> Fixture {
    Fixture::new(2, Arc::new(MemoryKv::new()))
}

fn wave_states(data: &Value) -> Vec<String> {
    data["waves"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["state"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn waves_run_in_order_the_epoch_advances_and_the_ledger_admits_verified_tenants() {
    let f = plain();
    let (status, created) = f.create(&[&[0], &[1, 2], &[3]], "k1").await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["data"]["state"], "planned");
    assert_eq!(created["data"]["target_version"], "2026.11.1");
    assert_eq!(
        wave_states(&created["data"]),
        ["pending", "pending", "pending"]
    );
    assert!(
        f.hosts.calls.lock().unwrap().is_empty(),
        "creating a rollout runs nothing"
    );
    assert_eq!(f.state.epoch().await.unwrap(), 0);

    f.pass().await;
    let data = f.get(&id).await;
    assert_eq!(
        (data["state"].as_str(), wave_states(&data)[0].as_str()),
        (Some("running"), "running")
    );
    assert_eq!(
        f.hosts.calls_of(&f.tenants[1], "plan"),
        0,
        "wave 2 waits for the canary's soak"
    );
    assert_eq!(
        f.state.epoch().await.unwrap(),
        1,
        "the epoch advanced when the first wave started"
    );
    f.passes(3).await;
    let data = f.get(&id).await;
    assert_eq!(data["state"], "done", "{data}");
    assert_eq!(wave_states(&data), ["done", "done", "done"]);
    // Order: canary, then wave 2, then wave 3; every tenant ran the four steps in order.
    let first: Vec<usize> = (0..4)
        .map(|i| f.hosts.first_call(&f.tenants[i]).unwrap())
        .collect();
    assert!(
        first[0] < first[1].min(first[2]) && first[1].max(first[2]) < first[3],
        "{first:?}"
    );
    for tenant in &f.tenants {
        for step in ["plan", "approve", "rehearse", "apply"] {
            assert_eq!(f.hosts.calls_of(tenant, step), 1, "{tenant} {step}");
        }
    }
    // The record carries the outcomes and the rescue pointers.
    let record = f.record(&id).await;
    assert_eq!(record["outcomes"][&f.tenants[0]]["ok"], true);
    assert_eq!(
        record["outcomes"][&f.tenants[0]]["rescue"],
        format!("spot migrate rescue {}", f.tenants[0])
    );
    // The skew decision is the library's over the recorded epochs.
    for tenant in &f.tenants {
        assert_eq!(f.state.may_sync(tenant).await.unwrap(), Ok(()));
    }
    let unknown = f
        .state
        .may_sync("00000000-0000-7000-8000-0000000000aa")
        .await
        .unwrap();
    assert_eq!(
        unknown,
        may_sync(EdgeEpoch::unverified(), Epoch::new(1), SkewLimit::new(1))
    );
    // Nothing is left for the runner.
    assert_eq!(rollouts::run_once(&f.state).await, 0);
}

#[tokio::test]
async fn a_canary_failure_pauses_the_fleet_and_only_resume_continues() {
    let f = plain();
    let (_, created) = f.create(&[&[0], &[1, 2], &[3]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    f.hosts.fail(&f.tenants[0], "rehearse");
    f.pass().await;
    assert_eq!(f.get(&id).await["state"], "paused");
    let record = f.record(&id).await;
    assert!(
        record["pause"]
            .as_str()
            .unwrap()
            .starts_with("canary_failure"),
        "{record}"
    );
    assert_eq!(record["outcomes"][&f.tenants[0]]["step"], "rehearse");
    assert_eq!(record["outcomes"][&f.tenants[0]]["ok"], false);
    // Time passes and the runner keeps running: no later wave starts, nothing re-runs.
    let calls = f.hosts.calls.lock().unwrap().len();
    f.passes(4).await;
    assert_eq!(f.hosts.calls.lock().unwrap().len(), calls);
    assert_eq!(f.get(&id).await["state"], "paused");
    // Resuming before the operator fixed anything runs the canary again, fails again, pauses again.
    let (status, body) = f.act(&id, "resume").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["state"], "paused");
    assert_eq!(f.hosts.calls_of(&f.tenants[0], "plan"), 2);
    assert_eq!(f.hosts.calls_of(&f.tenants[1], "plan"), 0);
    // After the operator action the same resume continues to the end.
    f.hosts.mend();
    let (status, body) = f.act(&id, "resume").await;
    assert_eq!(
        (status, body["data"]["state"].as_str()),
        (StatusCode::OK, Some("running"))
    );
    f.passes(4).await;
    assert_eq!(f.get(&id).await["state"], "done");
    assert_eq!(f.hosts.calls_of(&f.tenants[3], "apply"), 1);
    // A resume of a rollout that is not paused is a conflict.
    let (status, _) = f.act(&id, "resume").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_failure_in_a_later_wave_pauses_and_resume_skips_finished_tenants() {
    let f = plain();
    let (_, created) = f.create(&[&[0], &[1, 2], &[3]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    f.hosts.fail(&f.tenants[2], "apply");
    f.passes(2).await;
    let data = f.get(&id).await;
    assert_eq!(data["state"], "paused", "{data}");
    assert_eq!(wave_states(&data)[1], "failed");
    assert_eq!(
        f.hosts.calls_of(&f.tenants[3], "plan"),
        0,
        "wave 3 never started"
    );
    f.passes(3).await;
    assert_eq!(
        f.hosts.calls_of(&f.tenants[3], "plan"),
        0,
        "a pause never ends by itself"
    );
    f.hosts.mend();
    f.act(&id, "resume").await;
    f.passes(3).await;
    assert_eq!(f.get(&id).await["state"], "done");
    assert_eq!(
        f.hosts.calls_of(&f.tenants[1], "apply"),
        1,
        "the finished tenant is not run again"
    );
    assert_eq!(
        f.hosts.calls_of(&f.tenants[2], "apply"),
        2,
        "the failed one ran again"
    );
}

#[tokio::test]
async fn the_health_gate_pauses_on_a_new_red_but_not_on_one_that_was_there_before() {
    // New red: the migration of the canary turns it red.
    let f = plain();
    let (_, created) = f.create(&[&[0], &[1]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    f.hosts
        .turns_red
        .lock()
        .unwrap()
        .insert(f.tenants[0].clone());
    f.pass().await;
    assert_eq!(f.get(&id).await["state"], "paused");
    assert!(
        f.record(&id).await["pause"]
            .as_str()
            .unwrap()
            .contains("health")
    );
    assert_eq!(f.hosts.calls_of(&f.tenants[1], "plan"), 0);
    // Red before the wave: not this rollout's doing, it carries on.
    let g = plain();
    g.health
        .colours
        .lock()
        .unwrap()
        .insert(g.tenants[0].clone(), Health::Red);
    let (_, created) = g.create(&[&[0], &[1]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    g.passes(3).await;
    assert_eq!(g.get(&id).await["state"], "done");
}

#[tokio::test]
async fn an_unreadable_health_gate_never_lets_a_wave_pass() {
    let f = plain();
    let (_, created) = f.create(&[&[0], &[1]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    f.health.unreadable.store(true, Ordering::SeqCst);
    f.passes(3).await;
    assert_eq!(f.get(&id).await["state"], "paused");
    assert_eq!(f.hosts.calls_of(&f.tenants[1], "plan"), 0);
}

#[tokio::test]
async fn a_restart_pauses_a_running_rollout_and_resume_skips_what_finished() {
    let f = plain();
    let (_, created) = f.create(&[&[0], &[1, 2], &[3]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    f.pass().await;
    assert_eq!(f.get(&id).await["state"], "running");
    // The process dies: the machine is gone, the record stays.
    let g = f.restart();
    rollouts::run_once(&g.state).await;
    let data = g.get(&id).await;
    assert_eq!(data["state"], "paused", "{data}");
    assert!(
        g.record(&id).await["pause"]
            .as_str()
            .unwrap()
            .contains("restarted")
    );
    g.passes(2).await;
    assert_eq!(
        g.hosts.calls_of(&g.tenants[1], "plan"),
        0,
        "a restart never continues by itself"
    );
    let (status, _) = g.act(&id, "resume").await;
    assert_eq!(status, StatusCode::OK);
    g.passes(5).await;
    assert_eq!(g.get(&id).await["state"], "done");
    assert_eq!(
        g.hosts.calls_of(&g.tenants[0], "apply"),
        1,
        "the finished canary is not migrated again"
    );
}

#[tokio::test]
async fn an_operator_can_pause_a_running_rollout() {
    let f = plain();
    let (_, created) = f.create(&[&[0], &[1]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    f.pass().await;
    let (status, body) = f.act(&id, "pause").await;
    assert_eq!(
        (status, body["data"]["state"].as_str()),
        (StatusCode::OK, Some("paused"))
    );
    f.passes(3).await;
    assert_eq!(f.hosts.calls_of(&f.tenants[1], "plan"), 0);
    let (status, _) = f.act(&id, "pause").await;
    assert_eq!(status, StatusCode::CONFLICT, "already paused");
}

#[tokio::test]
async fn at_most_the_concurrency_limit_of_tenants_migrate_at_once() {
    let f = Fixture::new(2, Arc::new(MemoryKv::new()));
    let (_, created) = f.create(&[&[0], &[1, 2, 3]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    f.passes(4).await;
    assert_eq!(f.get(&id).await["state"], "done");
    let most = f.hosts.max_in_flight.load(Ordering::SeqCst);
    assert!(
        (1..=2).contains(&most),
        "{most} tenants in flight with a limit of 2"
    );
}

#[tokio::test]
async fn a_request_is_checked_before_anything_runs() {
    let f = plain();
    let t = |i: usize| f.tenants[i].clone();
    for (body, why) in [
        (json!({ "target_version": "1", "waves": [] }), "no waves"),
        (
            json!({ "target_version": "1", "waves": [{ "tenants": [] }] }),
            "an empty wave",
        ),
        (
            json!({ "target_version": "1", "waves": [{ "tenants": [t(0)] }, { "tenants": [t(0)] }] }),
            "a tenant twice",
        ),
        (
            json!({ "target_version": "1", "waves": [{ "tenants": ["00000000-0000-7000-8000-0000000000aa"] }] }),
            "an unknown tenant",
        ),
        (
            json!({ "target_version": "", "waves": [{ "tenants": [t(0)] }] }),
            "an empty version",
        ),
        (
            json!({ "target_version": "1", "waves": [{ "tenants": [t(0)] }], "extra": 1 }),
            "an unknown field",
        ),
    ] {
        let (status, answer) = f
            .send(
                "root",
                "POST",
                "/cp/v1/rollouts",
                &body.to_string(),
                Some("k"),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{why}: {answer}");
        assert_eq!(answer["error"]["code"], "cp.invalid", "{why}");
    }
    // A tenant that is not active.
    let id: TenantId = serde_json::from_value(json!(t(1))).unwrap();
    let mut leaving = f.store.get(&id).unwrap();
    leaving.state = TenantState::Deprovisioning;
    let version = leaving.version;
    f.store.update(leaving, version).unwrap();
    let (status, _) = f.create(&[&[1]], "k").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(f.hosts.calls.lock().unwrap().is_empty());
    assert!(
        f.kv.list("rollout").await.unwrap().is_empty(),
        "nothing was stored"
    );
}

#[tokio::test]
async fn the_idempotency_key_replays_and_one_rollout_is_active_at_a_time() {
    let f = plain();
    let (status, first) = f.create(&[&[0], &[1]], "k1").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, again) = f.create(&[&[0], &[1]], "k1").await;
    assert_eq!(
        (status, &again["data"]["id"]),
        (StatusCode::CREATED, &first["data"]["id"])
    );
    let (status, other) = f.create(&[&[0], &[2]], "k1").await;
    assert_eq!(
        (status, other["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("cp.replayed"))
    );
    let (status, _) = f
        .send("root", "POST", "/cp/v1/rollouts", &f.body(&[&[0]]), None)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Another rollout while the first is not finished.
    let (status, busy) = f.create(&[&[2], &[3]], "k2").await;
    assert_eq!(
        (status, busy["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("cp.conflict"))
    );
    // Once it is done the next one takes the next epoch.
    f.passes(3).await;
    assert_eq!(
        f.get(first["data"]["id"].as_str().unwrap()).await["state"],
        "done"
    );
    let (status, second) = f.create(&[&[2], &[3]], "k2").await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    f.passes(3).await;
    assert_eq!(f.state.epoch().await.unwrap(), 2);
    // The library's skew rule over the recorded epochs: tenants 0 and 1 are one epoch behind (the
    // limit is 1), a tenant that never verified is refused, and the library's simulation passes.
    assert_eq!(f.state.may_sync(&f.tenants[0]).await.unwrap(), Ok(()));
    let behind = may_sync(
        EdgeEpoch::verified(Epoch::new(1)),
        Epoch::new(3),
        SkewLimit::new(1),
    );
    assert!(behind.is_err());
    assert!(spot_fleet_rollout::run(SimConfig::default()).is_ok());
    let (_, list) = f.send("root", "GET", "/cp/v1/rollouts", "", None).await;
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn scope_hides_rollouts_and_only_an_all_scope_operator_changes_them() {
    let f = plain();
    let (_, created) = f.create(&[&[0], &[1]], "k1").await;
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    let path = format!("/cp/v1/rollouts/{id}");
    let (status, body) = f.send("alice", "GET", &path, "", None).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("cp.not_found"))
    );
    let (status, missing) = f
        .send("alice", "GET", "/cp/v1/rollouts/ro-nothing", "", None)
        .await;
    assert_eq!(
        (status, missing["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("cp.not_found"))
    );
    for verb in ["pause", "resume"] {
        let (status, _) = f
            .send("alice", "POST", &format!("{path}/{verb}"), "", None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{verb}");
    }
    let (status, _) = f
        .send(
            "alice",
            "POST",
            "/cp/v1/rollouts",
            &f.body(&[&[0]]),
            Some("a1"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, list) = f.send("alice", "GET", "/cp/v1/rollouts", "", None).await;
    assert!(list["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn postgres_rows_carry_the_same_rollout() {
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
        "DELETE FROM cp_tenants_kv WHERE ns LIKE 'rollout%'",
        "DELETE FROM cp_tenants_lease WHERE id LIKE 'rollout:%'",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    drop(client);
    let kv: Arc<dyn Kv> = Arc::new(spot_control_plane::modules::tenants::PgKv::new(pool));
    let f = Fixture::new(2, kv);
    let (status, created) = f.create(&[&[0], &[1]], "pg-k1").await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    f.passes(3).await;
    assert_eq!(
        f.get(created["data"]["id"].as_str().unwrap()).await["state"],
        "done"
    );
}
