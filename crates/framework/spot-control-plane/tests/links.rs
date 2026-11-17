//! The `links` module against a real PostgreSQL server through `SPOT_TEST_PG_URL` (skipped with a
//! printed reason when it is unset, never faked).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::sync::Arc;

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
tenants = ["t1"]
"#;

async fn plane() -> Option<(Router, Router, deadpool_postgres::Pool)> {
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
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
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
            name: "links",
            order: modules::links::ORDER,
            register: modules::links::register,
        }],
    })
    .unwrap();
    spot_control_plane::migrate(&pool, &built.migrations)
        .await
        .unwrap();
    let client = pool.get().await.unwrap();
    client
        .batch_execute("DELETE FROM cp_link_directory; DELETE FROM spot_control.link_registry")
        .await
        .unwrap();
    Some((
        built.host_router.clone(),
        built.operator_router.clone(),
        pool,
    ))
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

/// The tests share two tables of one database: they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const H1: &str = "aaaaaaaaaaaaaaaaaaaa";
const H2: &str = "bbbbbbbbbbbbbbbbbbbb";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_directory_leaks_no_name_before_consent_and_binds_the_tenant_to_the_certificate() {
    let _serial = SERIAL.lock().await;
    let Some((router, _operator, _pool)) = plane().await else {
        return;
    };
    let publish = |tenant: &'static str, handle: &'static str, extra: Value| {
        let router = router.clone();
        async move {
            let mut body =
                json!({ "handle": handle, "endpoint": format!("https://{tenant}.example.test") });
            if let (Some(map), Some(more)) = (body.as_object_mut(), extra.as_object()) {
                map.extend(more.clone());
            }
            send(
                &router,
                "PUT",
                "/cp/v1/host/links/endpoint",
                body,
                host(tenant),
            )
            .await
        }
    };
    assert_eq!(publish("t1", H1, json!({})).await.0, StatusCode::OK);
    // A name sent without opting in is not stored; asking for sharing without a name is invalid.
    assert_eq!(
        publish("t2", H2, json!({ "name": "Acme Ltd" })).await.0,
        StatusCode::OK
    );
    let (status, body) = send(
        &router,
        "GET",
        &format!("/cp/v1/host/links/directory/{H2}"),
        json!({}),
        host("t1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["endpoint"], "https://t2.example.test");
    assert!(
        body["data"].get("name").is_none(),
        "no name before the owner opts in"
    );
    assert_eq!(
        publish("t2", H2, json!({ "name": "Acme Ltd", "share_name": true }))
            .await
            .0,
        StatusCode::OK
    );
    let (_, body) = send(
        &router,
        "GET",
        &format!("/cp/v1/host/links/directory/{H2}"),
        json!({}),
        host("t1"),
    )
    .await;
    assert_eq!(body["data"]["name"], "Acme Ltd");
    // The body cannot name a tenant, and another tenant's handle cannot be taken.
    assert_eq!(
        publish("t1", H1, json!({ "tenant": "t2" })).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, body) = {
        let r = publish("t1", H2, json!({})).await;
        (r.0, r.1)
    };
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("cp.invalid"))
    );
    // Unknown handle and a plain-http endpoint.
    let (status, _) = send(
        &router,
        "GET",
        "/cp/v1/host/links/directory/cccccccccccccccccccc",
        json!({}),
        host("t1"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &router,
        "PUT",
        "/cp/v1/host/links/endpoint",
        json!({ "handle": H1, "endpoint": "http://insecure.example.test" }),
        host("t1"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operators_see_only_links_in_their_scope_and_hosts_only_their_own() {
    let _serial = SERIAL.lock().await;
    let Some((router, operator, pool)) = plane().await else {
        return;
    };
    let client = pool.get().await.unwrap();
    for (id, a, b) in [
        (
            "00000000-0000-7000-8000-000000000001",
            "tenant:t1",
            "tenant:t9",
        ),
        (
            "00000000-0000-7000-8000-000000000002",
            "tenant:t5",
            "tenant:t9",
        ),
    ] {
        client
            .execute(
                "INSERT INTO spot_control.link_registry (link_id, tenant_a, tenant_b, state) VALUES ($1::text::uuid, $2, $3, 'live')",
                &[&id, &a, &b],
            )
            .await
            .unwrap();
    }
    let (status, body) = send(
        &operator,
        "GET",
        "/cp/v1/links",
        json!({}),
        PeerCertificate::new("CN=alice", "fp-a"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    let (_, own) = send(
        &router,
        "GET",
        "/cp/v1/host/links/registry",
        json!({}),
        host("t9"),
    )
    .await;
    assert_eq!(own["data"].as_array().unwrap().len(), 2);
    assert!(
        own["data"][0].get("parties").is_none(),
        "a host sees ids and states only"
    );
    let (_, other) = send(
        &router,
        "GET",
        "/cp/v1/host/links/registry",
        json!({}),
        host("t7"),
    )
    .await;
    assert!(other["data"].as_array().unwrap().is_empty());
}
