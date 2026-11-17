//! The `usage` module against a real PostgreSQL server through `SPOT_TEST_PG_URL` (skipped with a
//! printed reason when it is unset, never faked).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_control_plane::auth::KeyVault;
use spot_control_plane::config::Config;
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, modules};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::ports::{AuditSink, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::MemoryNonceStore;
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;

struct Vault(String);

impl KeyVault for Vault {
    fn ca_key_pem(&self) -> Result<String, spot_control_plane::auth::CaError> {
        Ok(self.0.clone())
    }
}

const CONFIG: &str = r#"
region = "eu-1"
audience = "cp-eu-1"
administrator_keys = ["0000000000000000000000000000000000000000000000000000000000000001"]
[database]
host = "127.0.0.1"
dbname = "control"
user = "cp"
password_env = "UNUSED"
tls = false
[operator_listener]
listen = "127.0.0.1:0"
server_cert = "/x"
server_key = "/x"
client_ca = "/x/operator"
[host_listener]
listen = "127.0.0.1:1"
server_cert = "/x"
server_key = "/x"
client_ca = "/x/host"
[host_ca]
cert = "/x"
key = "/x"
common_name = "Spot host CA"
[[operators]]
subject = "CN=alice"
principal = "alice"
tenants = ["scoped-tenant"]
[[operators]]
subject = "CN=root"
principal = "root"
tenants = ["*"]
"#;

struct Plane {
    host: Router,
    operator: Router,
    pool: deadpool_postgres::Pool,
}

async fn plane() -> Option<Plane> {
    let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return None;
    };
    let mut pg = deadpool_postgres::Config::new();
    pg.url = Some(url);
    let pool = pg
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let built = ControlPlane::build(BuildDeps {
        config: Arc::new(Config::from_toml(CONFIG).unwrap()),
        pool: Some(pool.clone()),
        clock,
        audit,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(Vault(KeyPair::generate().unwrap().serialize_pem())),
        registrations: vec![Registration {
            name: "usage",
            order: modules::usage::ORDER,
            register: modules::usage::register,
        }],
    })
    .unwrap();
    spot_control_plane::migrate(&pool, &built.migrations)
        .await
        .unwrap();
    Some(Plane {
        host: built.host_router.clone(),
        operator: built.operator_router.clone(),
        pool,
    })
}

/// A tenant id no other run or test uses.
fn fresh_tenant() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "u{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

fn host(tenant: &str) -> PeerCertificate {
    PeerCertificate::new(
        &format!("CN=spot-host/{tenant}/h1"),
        &format!("fp-{tenant}"),
    )
}

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    body: Value,
    peer: PeerCertificate,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .extension(peer)
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn tx_event(id: &str, amount: u64) -> Value {
    json!({ "id": id, "meter": "transactions", "amount": amount, "at": NOW - 100 })
}

async fn set_quota(pool: &deadpool_postgres::Pool, tenant: &str, limit: i64, mode: &str) {
    let client = pool.get().await.unwrap();
    client
        .execute(
            "INSERT INTO cp_usage_quota (tenant, meter, limit_value, mode) VALUES ($1, 'transactions', $2, $3) \
             ON CONFLICT (tenant, meter) DO UPDATE SET limit_value = EXCLUDED.limit_value, mode = EXCLUDED.mode",
            &[&tenant, &limit, &mode],
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_event_id_is_counted_once() {
    let Some(p) = plane().await else { return };
    let tenant = fresh_tenant();
    let batch = json!({ "events": [tx_event("e1", 3), tx_event("e2", 4)] });
    let (status, first) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        batch.clone(),
        host(&tenant),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["data"]["accepted"], 2);
    let (_, second) = send(&p.host, "POST", "/cp/v1/host/usage", batch, host(&tenant)).await;
    assert_eq!(
        second["data"]["accepted"], 0,
        "a retried batch counts nothing new"
    );
    let (status, usage) = send(
        &p.operator,
        "GET",
        &format!("/cp/v1/usage?tenant={tenant}"),
        json!({}),
        PeerCertificate::new("CN=root", "fp-root"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(usage["data"]["meters"][0]["total"], 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_that_names_a_tenant_or_is_malformed_or_too_big_is_refused() {
    let Some(p) = plane().await else { return };
    let tenant = fresh_tenant();
    let (status, body) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        json!({ "tenant": "someone-else", "events": [tx_event("e1", 1)] }),
        host(&tenant),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("cp.invalid"))
    );
    assert_eq!(body["error"]["details"]["fields"], json!(["tenant"]));
    // An event naming a tenant, an unknown meter, and a personal-looking member are all refused.
    let (status, _) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        json!({ "events": [{ "id": "x", "meter": "transactions", "amount": 1, "at": NOW, "tenant": "t" }] }),
        host(&tenant),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, body) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        json!({ "events": [{ "id": "x", "meter": "bogus", "amount": 1, "at": NOW }] }),
        host(&tenant),
    )
    .await;
    assert_eq!(
        body["error"]["details"]["fields"],
        json!(["events[0].meter"])
    );
    let (_, body) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        json!({ "events": [{ "id": "x", "meter": "active_users", "member": "alice@example.com", "at": NOW }] }),
        host(&tenant),
    )
    .await;
    assert_eq!(
        body["error"]["details"]["fields"],
        json!(["events[0].member"])
    );
    // Over the event count and over the byte size.
    let many: Vec<Value> = (0..1001).map(|n| tx_event(&format!("m{n}"), 1)).collect();
    let (status, _) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        json!({ "events": many }),
        host(&tenant),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let big = json!({ "events": [tx_event(&"z".repeat(120), 1)], "pad": "x".repeat(300 * 1024) });
    let (status, body) = send(&p.host, "POST", "/cp/v1/host/usage", big, host(&tenant)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, Some("cp.body_too_large"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_exceed_is_answered_on_ingest_and_on_the_quota_endpoint() {
    let Some(p) = plane().await else { return };
    let tenant = fresh_tenant();
    set_quota(&p.pool, &tenant, 10, "hard").await;
    let (_, ok) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        json!({ "events": [tx_event("a", 5)] }),
        host(&tenant),
    )
    .await;
    assert_eq!(ok["data"]["quota"]["state"], "allowed");
    let (_, over) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        json!({ "events": [tx_event("b", 20)] }),
        host(&tenant),
    )
    .await;
    assert_eq!(over["data"]["quota"]["state"], "hard_exceeded");
    let (status, quota) = send(
        &p.host,
        "GET",
        "/cp/v1/host/quota",
        json!({}),
        host(&tenant),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(quota["data"]["state"], "hard_exceeded");
    assert_eq!(quota["data"]["meters"][0]["meter"], "transactions");
    assert_eq!(quota["data"]["meters"][0]["limit"], 10);
    // Another tenant's quota answer shows nothing of this one.
    let other = fresh_tenant();
    let (_, theirs) = send(&p.host, "GET", "/cp/v1/host/quota", json!({}), host(&other)).await;
    assert_eq!(theirs["data"]["state"], "allowed");
    assert_eq!(theirs["data"]["meters"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_ingests_of_the_same_event_converge() {
    let Some(p) = plane().await else { return };
    let tenant = fresh_tenant();
    let batch = json!({ "events": [tx_event("same", 9)] });
    let one = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        batch.clone(),
        host(&tenant),
    );
    let two = send(&p.host, "POST", "/cp/v1/host/usage", batch, host(&tenant));
    let ((s1, a), (s2, b)) = tokio::join!(one, two);
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    let accepted =
        a["data"]["accepted"].as_u64().unwrap() + b["data"]["accepted"].as_u64().unwrap();
    assert_eq!(accepted, 1, "exactly one of the two counted the event");
    let (_, usage) = send(
        &p.operator,
        "GET",
        &format!("/cp/v1/usage?tenant={tenant}"),
        json!({}),
        PeerCertificate::new("CN=root", "fp-root"),
    )
    .await;
    assert_eq!(usage["data"]["meters"][0]["total"], 9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_reads_only_tenants_in_scope() {
    let Some(p) = plane().await else { return };
    let alice = PeerCertificate::new("CN=alice", "fp-alice");
    let (status, body) = send(
        &p.operator,
        "GET",
        "/cp/v1/usage?tenant=another-tenant",
        json!({}),
        alice.clone(),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("cp.not_found"))
    );
    let (status, _) = send(
        &p.operator,
        "GET",
        "/cp/v1/usage?tenant=scoped-tenant",
        json!({}),
        alice.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&p.operator, "GET", "/cp/v1/usage", json!({}), alice).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // A host cannot read operator numbers.
    let (status, _) = send(
        &p.operator,
        "GET",
        "/cp/v1/usage?tenant=scoped-tenant",
        json!({}),
        host("t1"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_retention_job_compacts_reporting_tenants() {
    let Some(p) = plane().await else { return };
    let tenant = fresh_tenant();
    let (status, _) = send(
        &p.host,
        "POST",
        "/cp/v1/host/usage",
        json!({ "events": [tx_event("r1", 1)] }),
        host(&tenant),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let clock = ManualClock::new(time::OffsetDateTime::from_unix_timestamp(NOW).unwrap());
    let periods = spot_metering::TenantPeriods::new(
        spot_metering::PeriodSpec::calendar_month("UTC").unwrap(),
    );
    let compacted = modules::usage::run_retention_once(&p.pool, &clock, &periods)
        .await
        .unwrap();
    assert!(compacted >= 1);
}
