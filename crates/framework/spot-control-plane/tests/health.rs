//! The `health` module: ingest from a host certificate, the three colours and their
//! transitions, silence turning a node unknown (also across a restart), the series budget,
//! alert delivery, scope and the allow-list; plus a PostgreSQL smoke test through
//! `SPOT_TEST_PG_URL` (prints why it skips when unset).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_control_plane::auth::KeyVault;
use spot_control_plane::config::Config;
use spot_control_plane::modules::health::{self, HealthState, NotifySink};
use spot_control_plane::modules::tenants_kv::{Kv, MemoryKv, PgKv};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_fleet_health::{AlertNotification, RuleSet, Settings};
use spot_mgmt_api::ports::{AuditSink, BoxFuture, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::{MemoryNonceStore, SoftwareSigner};
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

#[derive(Default)]
struct RecordingSink(Mutex<Vec<AlertNotification>>);

impl NotifySink for RecordingSink {
    fn deliver<'a>(&'a self, n: &'a AlertNotification) -> BoxFuture<'a, Result<(), String>> {
        self.0.lock().unwrap().push(n.clone());
        Box::pin(std::future::ready(Ok(())))
    }
}

thread_local! {
    static STATE: RefCell<Option<Arc<HealthState>>> = const { RefCell::new(None) };
}

fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = STATE.with(|s| s.borrow().clone()).expect("state set");
    health::register_with(ctx, state);
    Ok(())
}

struct Fixture {
    plane: ControlPlane,
    clock: Arc<ManualClock>,
    sink: Arc<RecordingSink>,
}

fn fixture_over(kv: Arc<dyn Kv>, clock: Arc<ManualClock>, max_series: usize) -> Fixture {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let sink = Arc::new(RecordingSink::default());
    let sink_dyn: Arc<dyn NotifySink> = sink.clone();
    let state = HealthState::new(
        kv,
        clock_dyn.clone(),
        RuleSet::builtin().unwrap(),
        Settings::defaults(),
        max_series,
        Some(sink_dyn),
    )
    .unwrap();
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
            name: "health",
            order: health::ORDER,
            register,
        }],
    })
    .unwrap();
    Fixture { plane, clock, sink }
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    fixture_over(Arc::new(MemoryKv::new()), clock, 512)
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
    async fn beat(&self, tenant: &str, host: &str, items: &Value) -> (StatusCode, Value) {
        let body = json!({ "observations": items }).to_string();
        self.beat_raw(tenant, host, body).await
    }

    async fn beat_raw(&self, tenant: &str, host: &str, body: String) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/cp/v1/host/heartbeat")
            .extension(PeerCertificate::new(
                &format!("CN=spot-host/{tenant}/{host}"),
                &format!("fp-{host}"),
            ))
            .body(Body::from(body))
            .unwrap();
        answer(&self.plane.host_router, req).await
    }

    async fn get(&self, who: &str, path: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .uri(path)
            .extension(PeerCertificate::new(
                &format!("CN={who}"),
                &format!("fp-{who}"),
            ))
            .body(Body::empty())
            .unwrap();
        answer(&self.plane.operator_router, req).await
    }

    fn advance(&self, secs: u64) {
        self.clock.advance(std::time::Duration::from_secs(secs));
    }

    /// The health of the node `<tenant>:<host>` as the console reads it.
    async fn node(&self, tenant: &str, host: &str) -> Value {
        let (status, view) = self
            .get("root", &format!("/cp/v1/health/tenants/{tenant}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{view}");
        view["data"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == format!("{tenant}:{host}"))
            .unwrap()
            .clone()
    }
}

fn probe(value: i64) -> Value {
    json!([{ "signal": "health_probe", "value": value }])
}

#[tokio::test]
async fn a_reporting_host_is_green() {
    let f = fixture();
    assert_eq!(
        f.beat("tenant-one", "h1", &probe(1)).await.0,
        StatusCode::OK
    );
    let node = f.node("tenant-one", "h1").await;
    assert_eq!(node["health"], "green", "{node}");
    assert!(node["sentence"].as_str().unwrap().contains("normally"));
}

#[tokio::test]
async fn red_then_green_with_one_opened_and_one_resolved_notification() {
    let f = fixture();
    f.beat("tenant-one", "h1", &probe(0)).await;
    // The breach has not lasted 30 s yet.
    assert_eq!(f.node("tenant-one", "h1").await["health"], "green");
    f.advance(31);
    f.beat("tenant-one", "h1", &probe(0)).await;
    let node = f.node("tenant-one", "h1").await;
    assert_eq!(node["health"], "red", "{node}");
    assert!(node["sentence"].as_str().unwrap().contains("failing"));
    // A repeated red heartbeat does not notify again.
    f.advance(5);
    f.beat("tenant-one", "h1", &probe(0)).await;
    f.advance(5);
    f.beat("tenant-one", "h1", &probe(1)).await;
    assert_eq!(f.node("tenant-one", "h1").await["health"], "green");
    f.advance(61);
    f.beat("tenant-one", "h1", &probe(1)).await;
    let reasons: Vec<String> = f
        .sink
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|n| format!("{:?}", n.reason))
        .collect();
    assert_eq!(reasons, ["Opened", "Resolved"]);
}

#[tokio::test]
async fn a_lasting_error_rate_is_amber() {
    let f = fixture();
    let items = json!([
        { "signal": "error_rate", "value": 600 },
        { "signal": "health_probe", "value": 1 },
    ]);
    f.beat("tenant-one", "h1", &items).await;
    f.advance(121);
    f.beat("tenant-one", "h1", &items).await;
    let node = f.node("tenant-one", "h1").await;
    assert_eq!(node["health"], "amber", "{node}");
    assert_eq!(node["findings"][0]["rule"], "error_rate_high");
}

#[tokio::test]
async fn a_silent_node_turns_unknown_and_stays_known_across_a_restart() {
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let kv: Arc<dyn Kv> = Arc::new(MemoryKv::new());
    let f = fixture_over(Arc::clone(&kv), Arc::clone(&clock), 512);
    f.beat("tenant-one", "h1", &probe(1)).await;
    assert_eq!(f.node("tenant-one", "h1").await["health"], "green");
    f.advance(46);
    let node = f.node("tenant-one", "h1").await;
    assert_eq!(node["health"], "amber", "{node}");
    assert_eq!(node["stale"], true);
    assert!(node["sentence"].as_str().unwrap().contains("No telemetry"));
    // The service restarts: the host is still known, and still not healthy.
    let g = fixture_over(kv, clock, 512);
    let node = g.node("tenant-one", "h1").await;
    assert_eq!(node["health"], "amber", "{node}");
    assert_eq!(node["no_data"], true);
}

#[tokio::test]
async fn a_series_flood_is_rejected_and_counted_without_harming_another_tenant() {
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let f = fixture_over(Arc::new(MemoryKv::new()), clock, 10);
    let four = json!([
        { "signal": "health_probe", "value": 1 },
        { "signal": "error_rate", "value": 1 },
        { "signal": "disk_usage", "value": 100 },
        { "signal": "queue_lag", "value": 1 },
    ]);
    assert_eq!(f.beat("tenant-one", "h1", &four).await.0, StatusCode::OK);
    assert_eq!(f.beat("tenant-one", "h2", &four).await.0, StatusCode::OK);
    let (status, refused) = f.beat("tenant-one", "h3", &four).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["error"]["code"], "cp.cardinality_rejected");
    // Existing series keep working; another tenant has its own budget.
    assert_eq!(f.beat("tenant-one", "h1", &four).await.0, StatusCode::OK);
    assert_eq!(f.beat("tenant-two", "h1", &four).await.0, StatusCode::OK);
    let (_, view) = f.get("root", "/cp/v1/health").await;
    assert_eq!(view["data"]["cardinality_rejected"], 4);
    let (_, one) = f.get("root", "/cp/v1/health/tenants/tenant-one").await;
    assert_eq!(one["data"]["cardinality_rejected"], 4);
    let (_, two) = f.get("root", "/cp/v1/health/tenants/tenant-two").await;
    assert_eq!(two["data"]["cardinality_rejected"], 0);
}

#[tokio::test]
async fn an_operator_sees_only_the_tenants_in_scope() {
    let f = fixture();
    f.beat("tenant-one", "h1", &probe(1)).await;
    f.beat("tenant-two", "h1", &probe(1)).await;
    let (status, view) = f.get("alice", "/cp/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = view["data"]["subjects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["tenant_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["tenant-one"]);
    assert_eq!(
        f.get("alice", "/cp/v1/health/tenants/tenant-two").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn only_allow_listed_signals_and_fields_are_accepted() {
    let f = fixture();
    let cases = [
        json!({ "observations": [{ "signal": "customer_balance", "value": 1 }] }),
        json!({ "observations": [{ "signal": "health_probe", "value": 1, "customer": "acme" }] }),
        json!({ "observations": [{ "signal": "disk_usage", "value": 20_000 }] }),
        json!({ "observations": [{ "signal": "health_probe", "value": 1, "observed_at": NOW + 3_600 }] }),
        json!({ "observations": [{ "signal": "health_probe", "value": 1 }], "tenant": "tenant-two" }),
        json!({ "observations": (0..201).map(|_| json!({ "signal": "health_probe", "value": 1 })).collect::<Vec<_>>() }),
    ];
    for case in cases {
        let (status, answer) = f.beat_raw("tenant-one", "h1", case.to_string()).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{case} -> {answer}"
        );
    }
    // Nothing of it was stored: the host is still unknown.
    let (status, _) = f.get("root", "/cp/v1/health/tenants/tenant-one").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_over_postgresql() {
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
        "DELETE FROM cp_tenants_kv WHERE ns = 'health_subjects'",
    ] {
        client.batch_execute(statement).await.unwrap();
    }
    drop(client);
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let f = fixture_over(Arc::new(PgKv::new(pool)), clock, 512);
    assert_eq!(
        f.beat("tenant-one", "h1", &probe(1)).await.0,
        StatusCode::OK
    );
    assert_eq!(f.node("tenant-one", "h1").await["health"], "green");
}
