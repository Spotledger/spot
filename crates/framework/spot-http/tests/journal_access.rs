//! The journal access routes (contract 0.16.0, WP-JRN-09.T4): every status, `private, no-store`
//! on every answer, a segment streamed with its `ETag`, `Content-Length` and ranges, and the
//! path texts handed to the port raw so that a hostile path is the port's (audited) 404.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::ports::journal_access::{
    JournalAccessError, JournalAccessPort, JournalGeneration, JournalInfo, MemoryJournalAccess,
};
use spot_http::api::router::routes::{RouteState, journal_access};
use spot_http::api::{PortKey, Ports};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";

struct Fixture {
    router: axum::Router,
    port: Arc<MemoryJournalAccess>,
}

fn builder(state: RouteState) -> axum::Router {
    let acme = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(journal_access::routes().with_state(state))
    .build()
}

fn fixture() -> Fixture {
    let port = Arc::new(MemoryJournalAccess::new());
    port.allow("tok-auditor");
    port.refuse("tok-none", JournalAccessError::Denied { expired: false });
    port.refuse("tok-expired", JournalAccessError::Denied { expired: true });
    port.refuse(
        "tok-slow",
        JournalAccessError::RateLimited {
            retry_after_secs: 17,
        },
    );
    port.refuse("tok-down", JournalAccessError::Unavailable);
    port.set_info(JournalInfo {
        generations: vec![JournalGeneration {
            generation: 1,
            first_seq: 1,
            last_seq: 200,
            sealed_until_seq: 200,
            opened_at: "2026-11-01T00:00:00Z".to_owned(),
        }],
        policy: "standard".to_owned(),
        region: "eu".to_owned(),
    });
    port.set_manifest("1", json!({"generation": 1, "segments": []}));
    port.set_segment("1", "1-100", (0u8..=255).collect());
    let mut ports = Ports::new();
    ports.provide::<dyn JournalAccessPort>(port.clone());
    let keys: &[PortKey] = journal_access::required_ports();
    let router = builder(RouteState::new(ports.subset(keys).unwrap()));
    Fixture { router, port }
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
}

async fn get(router: &axum::Router, path: &str, token: Option<&str>, range: Option<&str>) -> Reply {
    let mut request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap()
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
    }
}

fn no_store(reply: &Reply) {
    assert_eq!(
        reply.headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
}

#[tokio::test]
async fn the_group_declares_the_three_operations_and_one_port() {
    assert_eq!(
        journal_access::SERVES,
        [
            "journal_info_get",
            "journal_manifest_get",
            "journal_segment_get"
        ]
    );
    assert_eq!(journal_access::required_ports().len(), 1);
}

#[tokio::test]
async fn info_answers_the_contract_shape() {
    let f = fixture();
    let r = get(&f.router, "/api/v1/journal", Some("tok-auditor"), None).await;
    assert_eq!(r.status, 200);
    no_store(&r);
    assert_eq!(
        r.json()["data"],
        json!({"generations": [{"generation": 1, "first_seq": 1, "last_seq": 200,
            "sealed_until_seq": 200, "opened_at": "2026-11-01T00:00:00Z"}],
            "policy": "standard", "region": "eu"})
    );
}

#[tokio::test]
async fn no_bearer_is_401_and_the_port_is_not_asked() {
    let f = fixture();
    let r = get(&f.router, "/api/v1/journal", None, None).await;
    assert_eq!(r.status, 401);
    assert!(f.port.seen().is_empty());
}

#[tokio::test]
async fn a_denial_is_one_403_and_only_expiry_adds_a_reason() {
    let f = fixture();
    for path in [
        "/api/v1/journal",
        "/api/v1/journal/manifest/1",
        "/api/v1/journal/segments/1/1-100",
    ] {
        let none = get(&f.router, path, Some("tok-none"), None).await;
        assert_eq!(none.status, 403, "{path}");
        no_store(&none);
        assert_eq!(none.json()["error"]["code"], "permission.denied");
        assert!(none.json()["error"]["details"].get("reason").is_none());
        let expired = get(&f.router, path, Some("tok-expired"), None).await;
        assert_eq!(expired.status, 403);
        assert_eq!(expired.json()["error"]["code"], "permission.denied");
        assert_eq!(
            expired.json()["error"]["details"]["reason"],
            "grant_expired"
        );
    }
}

#[tokio::test]
async fn limits_and_outages() {
    let f = fixture();
    let slow = get(
        &f.router,
        "/api/v1/journal/segments/1/1-100",
        Some("tok-slow"),
        None,
    )
    .await;
    assert_eq!(slow.status, 429);
    assert_eq!(slow.json()["error"]["code"], "api.rate_limited");
    assert_eq!(slow.headers.get(header::RETRY_AFTER).unwrap(), "17");
    no_store(&slow);
    let down = get(&f.router, "/api/v1/journal", Some("tok-down"), None).await;
    assert_eq!(down.status, 503);
    assert_eq!(down.json()["error"]["code"], "server.unavailable");
    assert!(down.headers.get(header::RETRY_AFTER).is_some());
}

#[tokio::test]
async fn the_manifest_and_a_missing_generation() {
    let f = fixture();
    let ok = get(
        &f.router,
        "/api/v1/journal/manifest/1",
        Some("tok-auditor"),
        None,
    )
    .await;
    assert_eq!(ok.status, 200);
    no_store(&ok);
    assert_eq!(ok.json()["generation"], 1);
    let missing = get(
        &f.router,
        "/api/v1/journal/manifest/9",
        Some("tok-auditor"),
        None,
    )
    .await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["error"]["code"], "journal.range_unavailable");
    no_store(&missing);
}

#[tokio::test]
async fn a_segment_streams_with_etag_length_and_ranges() {
    let f = fixture();
    let full = get(
        &f.router,
        "/api/v1/journal/segments/1/1-100",
        Some("tok-auditor"),
        None,
    )
    .await;
    assert_eq!(full.status, 200);
    no_store(&full);
    assert_eq!(full.bytes, (0u8..=255).collect::<Vec<u8>>());
    assert_eq!(full.headers.get(header::CONTENT_LENGTH).unwrap(), "256");
    assert_eq!(full.headers.get(header::ETAG).unwrap(), "\"fake\"");
    assert_eq!(
        full.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    assert_eq!(
        full.headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );

    let part = get(
        &f.router,
        "/api/v1/journal/segments/1/1-100",
        Some("tok-auditor"),
        Some("bytes=10-19"),
    )
    .await;
    assert_eq!(part.status, 206);
    assert_eq!(part.bytes, (10u8..=19).collect::<Vec<u8>>());
    assert_eq!(
        part.headers.get(header::CONTENT_RANGE).unwrap(),
        "bytes 10-19/256"
    );
    assert_eq!(part.headers.get(header::CONTENT_LENGTH).unwrap(), "10");

    let refused = get(
        &f.router,
        "/api/v1/journal/segments/1/1-100",
        Some("tok-auditor"),
        Some("bytes=9000-9001"),
    )
    .await;
    assert_eq!(refused.status, 416);
    assert_eq!(
        refused.headers.get(header::CONTENT_RANGE).unwrap(),
        "bytes */256"
    );
    assert!(refused.bytes.is_empty());
    no_store(&refused);
}

#[tokio::test]
async fn hostile_and_unlisted_paths_reach_the_port_as_raw_text_and_are_404() {
    let f = fixture();
    for path in [
        "/api/v1/journal/segments/1/1-99",
        "/api/v1/journal/segments/2/1-100",
        "/api/v1/journal/segments/%2e%2e/1-100",
        "/api/v1/journal/segments/1/01-100",
        "/api/v1/journal/segments/1/1-100-5",
    ] {
        let r = get(&f.router, path, Some("tok-auditor"), None).await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(r.json()["error"]["code"], "journal.range_unavailable");
        no_store(&r);
    }
    let seen = f.port.seen();
    assert_eq!(
        seen.len(),
        5,
        "every one reached the port (and so its audit)"
    );
    assert_eq!(
        seen[2].path,
        ["..", "1-100"],
        "decoded once, passed as text, never joined into a key"
    );
}

#[tokio::test]
async fn a_site_without_the_port_answers_501_not_configured() {
    let router = builder(RouteState::new(Ports::new()));
    let r = get(&router, "/api/v1/journal", Some("tok-auditor"), None).await;
    assert_eq!(r.status, 501);
    assert_eq!(r.json()["error"]["code"], "service.not_configured");
    no_store(&r);
}
