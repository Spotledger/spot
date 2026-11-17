//! The `desired_state` module over the in-memory rows with a fake agent that answers queued
//! commands: drift and its pending plan, silence, approval with pre-signed commands, the
//! canary-first run that halts on a failure, scope; plus a PostgreSQL smoke test through
//! `SPOT_TEST_PG_URL` (prints why it skips when unset).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::RefCell;
use std::collections::BTreeSet;
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
use spot_control_plane::modules::desired_state::{self, DesiredStateState, Tuning};
use spot_control_plane::modules::tenants_kv::{Kv, MemoryKv, PgKv};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_desired_state::{DesiredState, NodeId, ObservedSnapshot, ParamValue, SettingKey};
use spot_mgmt_api::ports::{AuditSink, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_settings::{RegistryBuilder, SettingsRegistry, load_seed_catalogue};
use spot_signed_command::{Command, MemoryNonceStore, Signer, SoftwareSigner, sign};
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;
const LEVEL: &str = "runtime.logging.level";

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
subject = "CN=bob"
principal = "bob"
tenants = ["*"]
[[operators]]
subject = "CN=alice"
principal = "alice"
tenants = ["tenant-one"]
"#
    )
}

fn registry() -> SettingsRegistry {
    RegistryBuilder::new()
        .register_all(load_seed_catalogue().unwrap())
        .build()
        .unwrap()
}

thread_local! {
    static STATE: RefCell<Option<Arc<DesiredStateState>>> = const { RefCell::new(None) };
}

fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = STATE.with(|s| s.borrow().clone()).expect("state set");
    desired_state::register_with(ctx, state);
    Ok(())
}

struct Fixture {
    plane: ControlPlane,
    clock: Arc<ManualClock>,
    signer: SoftwareSigner,
    kv: Arc<dyn Kv>,
}

fn fixture_over(kv: Arc<dyn Kv>) -> Fixture {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let tuning = Tuning {
        tick_ms: 1,
        verify_attempts: 3_000,
        verify_interval_ms: 1,
    };
    let state = DesiredStateState::new(Arc::clone(&kv), clock_dyn.clone(), registry(), tuning);
    STATE.with(|s| *s.borrow_mut() = Some(state));
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let ca = KeyPair::generate().unwrap().serialize_pem();
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: None,
        clock: clock_dyn,
        audit,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(ca)),
        registrations: vec![Registration {
            name: "desired_state",
            order: desired_state::ORDER,
            register,
        }],
    })
    .unwrap();
    Fixture {
        plane,
        clock,
        signer,
        kv,
    }
}

fn fixture() -> Fixture {
    fixture_over(Arc::new(MemoryKv::new()))
}

async fn answer(router: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

impl Fixture {
    fn envelope(&self, op: &str, audience: &str) -> String {
        let now = self.clock.now_utc().unix_timestamp();
        let command = Command::new(
            op,
            json!({ "note": "x" }),
            audience,
            self.signer.key_id(),
            now - 5,
            now + 300,
        );
        let signed = sign(&command, &self.signer).unwrap();
        let text = spot_mgmt_api::wire::encode(&signed, &[]).unwrap();
        URL_SAFE_NO_PAD.encode(text.as_bytes())
    }

    async fn op(&self, who: &str, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .extension(PeerCertificate::new(
                &format!("CN={who}"),
                &format!("fp-{who}"),
            ));
        if method != "GET" {
            let now = self.clock.now_utc().unix_timestamp();
            let command = Command::new(
                "cp.test",
                json!({ "body_sha256": sha256_hex(body.as_bytes()), "path": path }),
                "cp-eu-1",
                self.signer.key_id(),
                now - 5,
                now + 300,
            );
            let signed = sign(&command, &self.signer).unwrap();
            let text = spot_mgmt_api::wire::encode(&signed, &[]).unwrap();
            b = b.header("x-spot-command", URL_SAFE_NO_PAD.encode(text.as_bytes()));
        }
        answer(
            &self.plane.operator_router,
            b.body(Body::from(body.to_owned())).unwrap(),
        )
        .await
    }

    async fn report(&self, tenant: &str, server: &str, level: &str) -> (StatusCode, Value) {
        let snapshot = ObservedSnapshot::new(0).with_setting(
            SettingKey::new(LEVEL).unwrap(),
            ParamValue::Text(level.to_owned()),
        );
        let req = Request::builder()
            .method("POST")
            .uri("/cp/v1/host/desired-state/report")
            .extension(PeerCertificate::new(
                &format!("CN=spot-host/{tenant}/{server}"),
                "fp-host",
            ))
            .body(Body::from(serde_json::to_vec(&snapshot).unwrap()))
            .unwrap();
        answer(&self.plane.host_router, req).await
    }

    /// A group `prod` of `servers` that wants the logging level at `warn`.
    async fn put_group(
        &self,
        who: &str,
        tenants: &[&str],
        servers: &[&str],
    ) -> (StatusCode, Value) {
        let mut layer = DesiredState::for_group("prod").with_setting(
            SettingKey::new(LEVEL).unwrap(),
            ParamValue::Text("warn".to_owned()),
        );
        for server in servers {
            layer = layer.with_server(NodeId::new(*server).unwrap());
        }
        let body = json!({ "tenants": tenants, "layers": [layer] }).to_string();
        self.op(who, "PUT", "/cp/v1/groups/prod", &body).await
    }

    async fn plans(&self, who: &str) -> Vec<Value> {
        let (status, plans) = self.op(who, "GET", "/cp/v1/groups/prod/plans", "").await;
        assert_eq!(status, StatusCode::OK, "{plans}");
        plans["data"].as_array().unwrap().clone()
    }

    /// The approval body: one envelope per operation of each step, signed by the administrator.
    fn approval(&self, plan: &Value, audience_override: Option<&str>) -> String {
        let steps: Vec<Value> = plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| {
                let server = audience_override.unwrap_or_else(|| step["server"].as_str().unwrap());
                let envelopes: Vec<String> = step["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|op| self.envelope(op["id"].as_str().unwrap(), server))
                    .collect();
                json!({ "step": step["id"], "envelopes": envelopes })
            })
            .collect();
        json!({ "commands": steps }).to_string()
    }

    async fn wait_state(&self, digest: &str, wanted: &[&str]) -> Value {
        for _ in 0..4_000 {
            let (_, plan) = self
                .op(
                    "root",
                    "GET",
                    &format!("/cp/v1/groups/prod/plans/{digest}"),
                    "",
                )
                .await;
            if wanted.contains(&plan["data"]["state"].as_str().unwrap_or("")) {
                return plan["data"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the plan never reached {wanted:?}");
    }
}

/// A fake agent: answers every queued command, failing those for `failing` servers.
fn spawn_agent(
    kv: Arc<dyn Kv>,
    failing: Arc<Mutex<BTreeSet<String>>>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !stop.load(Ordering::SeqCst) {
            for (key, _) in kv.list("agent_cmd").await.unwrap() {
                let (server, id) = key.split_once('/').unwrap();
                if kv.get("agent_result", id).await.unwrap().is_none() {
                    let bad = failing.lock().unwrap().contains(server);
                    kv.put("agent_result", id, if bad { "failed" } else { "ok" })
                        .await
                        .unwrap();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
}

const SERVERS: [&str; 3] = ["srv-001", "srv-002", "srv-003"];

async fn drifted_fixture() -> Fixture {
    let f = fixture();
    assert_eq!(
        f.put_group("root", &["tenant-one"], &SERVERS).await.0,
        StatusCode::OK
    );
    for server in SERVERS {
        assert_eq!(
            f.report("tenant-one", server, "debug").await.0,
            StatusCode::OK
        );
    }
    f
}

#[tokio::test]
async fn drift_is_detected_and_a_pending_plan_is_listed_but_nothing_is_queued() {
    let f = fixture();
    assert_eq!(
        f.put_group("root", &["tenant-one"], &SERVERS).await.0,
        StatusCode::OK
    );
    f.report("tenant-one", "srv-001", "debug").await;
    f.report("tenant-one", "srv-002", "warn").await;
    f.report("tenant-one", "srv-003", "debug").await;
    let (_, drift) = f.op("root", "GET", "/cp/v1/groups/prod/drift", "").await;
    let rows = drift["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{drift}");
    assert!(rows.iter().all(|r| r["state"] == "drifted"));
    let plans = f.plans("root").await;
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0]["state"], "pending");
    assert_eq!(plans[0]["size"], 2);
    assert!(f.kv.list("agent_cmd").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_silent_server_turns_unknown_after_the_deadline() {
    let f = fixture();
    f.put_group("root", &["tenant-one"], &SERVERS[..2]).await;
    f.report("tenant-one", "srv-001", "warn").await;
    f.report("tenant-one", "srv-002", "warn").await;
    let (_, none) = f.op("root", "GET", "/cp/v1/groups/prod/drift", "").await;
    assert!(none["data"].as_array().unwrap().is_empty(), "{none}");
    f.clock.advance(std::time::Duration::from_secs(301));
    let (_, both) = f.op("root", "GET", "/cp/v1/groups/prod/drift", "").await;
    let rows = both["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["state"] == "unknown"), "{both}");
    f.report("tenant-one", "srv-001", "warn").await;
    let (_, one) = f.op("root", "GET", "/cp/v1/groups/prod/drift", "").await;
    let rows = one["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["server"], "srv-002");
}

#[tokio::test]
async fn nothing_runs_until_approved_and_the_requester_cannot_approve() {
    let f = drifted_fixture().await;
    let digest = f.plans("root").await[0]["digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let base = format!("/cp/v1/groups/prod/plans/{digest}");
    // Not approved: rehearse and run are refused, nothing is queued.
    assert_eq!(
        f.op("root", "POST", &format!("{base}/rehearse"), "{}")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.op("root", "POST", &format!("{base}/run"), "{}").await.0,
        StatusCode::FORBIDDEN
    );
    let plan = f.plans("root").await[0].clone();
    // A command addressed to another agent is refused.
    let wrong = f.approval(&plan, Some("srv-999"));
    let (status, _) = f
        .op("bob", "POST", &format!("{base}/approve"), &wrong)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // The requester (root asks for the plan) cannot approve it.
    assert_eq!(
        f.op("root", "POST", "/cp/v1/groups/prod/plans", "{}")
            .await
            .0,
        StatusCode::OK
    );
    let own = f.approval(&plan, None);
    let (status, answer) = f.op("root", "POST", &format!("{base}/approve"), &own).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    let (status, answer) = f.op("bob", "POST", &format!("{base}/approve"), &own).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["data"]["state"], "approved");
    // Approved but not rehearsed: still no run.
    assert_eq!(
        f.op("root", "POST", &format!("{base}/run"), "{}").await.0,
        StatusCode::FORBIDDEN
    );
    assert!(f.kv.list("agent_cmd").await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approved_rehearsed_plan_runs_canary_first_and_completes() {
    let f = drifted_fixture().await;
    f.op("root", "POST", "/cp/v1/groups/prod/plans", "{}").await;
    let plan = f.plans("root").await[0].clone();
    let digest = plan["digest"].as_str().unwrap().to_owned();
    let base = format!("/cp/v1/groups/prod/plans/{digest}");
    let body = f.approval(&plan, None);
    assert_eq!(
        f.op("bob", "POST", &format!("{base}/approve"), &body)
            .await
            .0,
        StatusCode::OK
    );
    let (status, rehearsal) = f.op("bob", "POST", &format!("{base}/rehearse"), "{}").await;
    assert_eq!(status, StatusCode::OK, "{rehearsal}");
    assert_eq!(rehearsal["data"]["passed"], true);
    let stop = Arc::new(AtomicBool::new(false));
    let agent = spawn_agent(Arc::clone(&f.kv), Arc::default(), Arc::clone(&stop));
    let (status, started) = f.op("bob", "POST", &format!("{base}/run"), "{}").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    let done = f
        .wait_state(&digest, &["complete", "halted", "aborted"])
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(f.kv.list("agent_cmd").await.unwrap().len(), 3);
    stop.store(true, Ordering::SeqCst);
    agent.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_canary_halts_the_run_and_no_later_wave_starts() {
    let f = drifted_fixture().await;
    f.op("root", "POST", "/cp/v1/groups/prod/plans", "{}").await;
    let plan = f.plans("root").await[0].clone();
    let digest = plan["digest"].as_str().unwrap().to_owned();
    let base = format!("/cp/v1/groups/prod/plans/{digest}");
    let body = f.approval(&plan, None);
    f.op("bob", "POST", &format!("{base}/approve"), &body).await;
    f.op("bob", "POST", &format!("{base}/rehearse"), "{}").await;
    let failing = Arc::new(Mutex::new(
        SERVERS.iter().map(|s| (*s).to_owned()).collect(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let agent = spawn_agent(Arc::clone(&f.kv), failing, Arc::clone(&stop));
    f.op("bob", "POST", &format!("{base}/run"), "{}").await;
    let done = f
        .wait_state(&digest, &["complete", "halted", "aborted"])
        .await;
    assert_eq!(done["state"], "halted", "{done}");
    // Only the canary's command was ever queued.
    assert_eq!(f.kv.list("agent_cmd").await.unwrap().len(), 1);
    stop.store(true, Ordering::SeqCst);
    agent.await.unwrap();
}

#[tokio::test]
async fn one_groups_desired_state_is_invisible_to_another_scope() {
    let f = fixture();
    assert_eq!(
        f.put_group("root", &["tenant-two"], &SERVERS).await.0,
        StatusCode::OK
    );
    // alice is scoped to tenant-one only.
    for path in [
        "/cp/v1/groups/prod",
        "/cp/v1/groups/prod/drift",
        "/cp/v1/groups/prod/plans",
    ] {
        assert_eq!(
            f.op("alice", "GET", path, "").await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    let (_, list) = f.op("alice", "GET", "/cp/v1/groups", "").await;
    assert!(list["data"].as_array().unwrap().is_empty());
    assert_eq!(
        f.put_group("alice", &["tenant-two"], &SERVERS).await.0,
        StatusCode::NOT_FOUND
    );
    // A host of another tenant cannot report into the group.
    assert_eq!(
        f.report("tenant-one", "srv-001", "debug").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.report("tenant-two", "srv-001", "debug").await.0,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drift_over_postgresql() {
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
        "DELETE FROM cp_tenants_kv WHERE ns LIKE 'ds\\_%' OR ns LIKE 'agent\\_%'",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    drop(client);
    let f = fixture_over(Arc::new(PgKv::new(pool)));
    f.put_group("root", &["tenant-one"], &SERVERS).await;
    f.report("tenant-one", "srv-001", "debug").await;
    let (_, drift) = f.op("root", "GET", "/cp/v1/groups/prod/drift", "").await;
    // srv-001 drifted; the two servers that never reported are listed as silent (state unknown).
    let rows = drift["data"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{drift}");
    assert_eq!(
        rows.iter().filter(|r| r["state"] == "drifted").count(),
        1,
        "{drift}"
    );
    assert_eq!(
        rows.iter().filter(|r| r["state"] == "unknown").count(),
        2,
        "{drift}"
    );
    assert_eq!(f.plans("root").await.len(), 1);
}
