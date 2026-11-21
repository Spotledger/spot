//! `run_start` (`POST /api/v1/process/{process}/runs`, WP-BPM-02.T5) over [`MemoryProcessPort`]:
//! the `201` and the `200` replay of an `Idempotency-Key`, the conflict, the scope of a key (caller
//! and site), the release of a refused start, the shape and bounds of the closed body, and the
//! refusals (an unknown process and one the caller may not start are one answer; invalid input
//! carries field paths and never a value; the starter is always the authenticated caller).
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::idempotency::{IdempotencyStore, MemoryIdempotencyStore};
use spot_http::api::ports::process::{MemoryProcessPort, ProcessDef, ProcessPort};
use spot_http::api::router::routes::{RouteState, process};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const HOST_B: &str = "erp.beta.com";
const ACME: &str = "acme";
const BETA: &str = "beta";

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    bytes: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_slice(&self.bytes).unwrap_or(Json::Null)
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
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn run_id(&self) -> String {
        self.json()["data"]["id"].as_str().unwrap_or("").to_owned()
    }
}

/// A reply with the per-request id removed, to compare two answers.
fn strip(reply: &Reply) -> (u16, Json, Option<String>, Option<String>) {
    let mut body = reply.json();
    if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
        error.remove("request_id");
    }
    (
        reply.status,
        body,
        reply.header("content-type").map(str::to_owned),
        reply.header("cache-control").map(str::to_owned),
    )
}

struct Fixture {
    router: axum::Router,
    port: Arc<MemoryProcessPort>,
    store: Arc<MemoryIdempotencyStore>,
}

fn sites() -> StaticSites {
    let acme = SiteId::new(ACME).unwrap();
    let beta = SiteId::new(BETA).unwrap();
    StaticSites::new()
        .add(HOST, acme.clone())
        .add(HOST_B, beta.clone())
        .add_site_name(acme)
        .add_site_name(beta)
}

fn router_over(ports: &Ports) -> axum::Router {
    let state = RouteState::new(ports.subset(process::required_ports()).unwrap());
    ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(process::routes().with_state(state))
        .build()
}

fn fixture() -> Fixture {
    let port = Arc::new(MemoryProcessPort::new());
    for site in [ACME, BETA] {
        port.define(
            site,
            ProcessDef::open("expense_claim", "Expense claim", &["amount"]),
        );
    }
    port.define(
        ACME,
        ProcessDef::open("payroll_run", "Payroll run", &["period"]).only_for(&["carol"]),
    );
    port.grant_supervise(ACME, "sue");
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let store = Arc::new(MemoryIdempotencyStore::new(clock));
    let mut ports = Ports::new();
    ports.provide::<dyn ProcessPort>(port.clone());
    ports.provide::<dyn IdempotencyStore>(store.clone());
    Fixture {
        router: router_over(&ports),
        port,
        store,
    }
}

async fn raw(
    router: &axum::Router,
    method: &str,
    host: &str,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let mut has_content_type = false;
    for (name, value) in extra {
        if name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        builder = builder.header(*name, *value);
    }
    if body.is_some() && !has_content_type {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let request = builder
        .body(body.map_or_else(Body::empty, Body::from))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
    }
}

async fn get(f: &Fixture, path: &str, token: &str) -> Reply {
    raw(&f.router, "GET", HOST, path, Some(token), &[], None).await
}

async fn get_on(f: &Fixture, host: &str, path: &str, token: &str) -> Reply {
    raw(&f.router, "GET", host, path, Some(token), &[], None).await
}

async fn start_with(
    f: &Fixture,
    token: &str,
    process: &str,
    key: &str,
    body: &Json,
    extra: &[(&str, &str)],
) -> Reply {
    let mut headers: Vec<(&str, &str)> = vec![("idempotency-key", key)];
    headers.extend_from_slice(extra);
    raw(
        &f.router,
        "POST",
        HOST,
        &format!("/api/v1/process/{process}/runs"),
        Some(token),
        &headers,
        Some(serde_json::to_vec(body).unwrap()),
    )
    .await
}

async fn start(f: &Fixture, token: &str, process: &str, key: &str, body: &Json) -> Reply {
    start_with(f, token, process, key, body, &[]).await
}

/// Starts an `expense_claim` as `token` and returns the run id.
async fn started(f: &Fixture, token: &str, key: &str) -> String {
    let reply = start(
        f,
        token,
        "expense_claim",
        key,
        &json!({"input": {"amount": "10.00"}}),
    )
    .await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    reply.run_id()
}

async fn cancel(f: &Fixture, token: &str, id: &str, body: &Json) -> Reply {
    raw(
        &f.router,
        "POST",
        HOST,
        &format!("/api/v1/process/runs/{id}/cancel"),
        Some(token),
        &[],
        Some(serde_json::to_vec(body).unwrap()),
    )
    .await
}

// ------------------------------------------------------------------------------------------
// run_start

#[tokio::test]
async fn run_start_answers_201_with_the_run_and_a_replay_answers_200_with_the_first_run() {
    let f = fixture();
    let body = json!({"input": {"amount": "125.50"}, "business_key": "EXP-2026-0042"});
    let first = start(&f, "alice", "expense_claim", "key-1", &body).await;
    assert_eq!(first.status, 201, "{}", first.text());
    assert_eq!(first.header("cache-control"), Some("private, no-store"));
    assert_eq!(first.header("idempotency-replayed"), None);
    let run = first.json()["data"].clone();
    assert_eq!(run["process"], "expense_claim");
    assert_eq!(run["version"], 1);
    assert_eq!(run["state"], "running");
    assert_eq!(run["business_key"], "EXP-2026-0042");
    assert!(run["started_at"].is_string());
    assert!(run.get("ended_at").is_none());
    assert!(run.get("failure_code").is_none());
    assert_eq!(run["steps"][0]["state"], "active");
    assert_eq!(run["documents"], json!([]));
    // The answer carries no variables, no outputs and no input.
    let text = first.text();
    assert!(
        !text.contains("125.50"),
        "the input is never echoed: {text}"
    );
    assert!(run.get("input").is_none() && run.get("variables").is_none());

    let replay = start(&f, "alice", "expense_claim", "key-1", &body).await;
    assert_eq!(replay.status, 200, "{}", replay.text());
    assert_eq!(replay.header("idempotency-replayed"), Some("true"));
    assert_eq!(replay.json()["data"], first.json()["data"]);
    assert_eq!(
        f.port.run_count(ACME),
        1,
        "a replay never starts a second run"
    );
}

#[tokio::test]
async fn run_start_without_a_business_key_answers_null() {
    let f = fixture();
    let reply = start(
        &f,
        "alice",
        "expense_claim",
        "key-nobk",
        &json!({"input": {"amount": "1"}}),
    )
    .await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    assert_eq!(reply.json()["data"]["business_key"], Json::Null);
}

#[tokio::test]
async fn the_same_key_with_a_different_body_or_process_is_a_conflict() {
    let f = fixture();
    let first = start(
        &f,
        "carol",
        "expense_claim",
        "key-2",
        &json!({"input": {"amount": "1"}}),
    )
    .await;
    assert_eq!(first.status, 201);
    let other_body = start(
        &f,
        "carol",
        "expense_claim",
        "key-2",
        &json!({"input": {"amount": "2"}}),
    )
    .await;
    assert_eq!(other_body.status, 409, "{}", other_body.text());
    assert_eq!(other_body.code(), "api.idempotency_conflict");
    // The process name is part of what the key vouches for.
    let other_process = start(
        &f,
        "carol",
        "payroll_run",
        "key-2",
        &json!({"input": {"period": "2026-10"}}),
    )
    .await;
    assert_eq!(other_process.status, 409, "{}", other_process.text());
    assert_eq!(other_process.code(), "api.idempotency_conflict");
    assert_eq!(f.port.run_count(ACME), 1);
}

#[tokio::test]
async fn a_key_is_scoped_to_the_caller_and_the_site() {
    let f = fixture();
    let body = json!({"input": {"amount": "5"}});
    let alice = start(&f, "alice", "expense_claim", "shared-key", &body).await;
    let bob = start(&f, "bob", "expense_claim", "shared-key", &body).await;
    assert_eq!(alice.status, 201);
    assert_eq!(
        bob.status,
        201,
        "bob's start is not alice's replay: {}",
        bob.text()
    );
    assert_ne!(alice.run_id(), bob.run_id());
    // The same key and body on the other site is a fresh start there.
    let beta = raw(
        &f.router,
        "POST",
        HOST_B,
        "/api/v1/process/expense_claim/runs",
        Some("alice"),
        &[("idempotency-key", "shared-key")],
        Some(serde_json::to_vec(&body).unwrap()),
    )
    .await;
    assert_eq!(beta.status, 201, "{}", beta.text());
    assert_eq!(f.port.run_count(ACME), 2);
    assert_eq!(f.port.run_count(BETA), 1);
}

#[tokio::test]
async fn a_refused_start_releases_the_key_so_a_corrected_retry_runs() {
    let f = fixture();
    let bad = start(
        &f,
        "alice",
        "expense_claim",
        "key-fix",
        &json!({"input": {}}),
    )
    .await;
    assert_eq!(bad.status, 422, "{}", bad.text());
    let good = start(
        &f,
        "alice",
        "expense_claim",
        "key-fix",
        &json!({"input": {"amount": "3"}}),
    )
    .await;
    assert_eq!(good.status, 201, "{}", good.text());
    assert_eq!(good.header("idempotency-replayed"), None);
}

#[tokio::test]
async fn run_start_needs_a_well_formed_idempotency_key() {
    let f = fixture();
    let body = serde_json::to_vec(&json!({"input": {"amount": "1"}})).unwrap();
    let path = "/api/v1/process/expense_claim/runs";
    let missing = raw(
        &f.router,
        "POST",
        HOST,
        path,
        Some("alice"),
        &[],
        Some(body.clone()),
    )
    .await;
    assert_eq!(missing.status, 400, "{}", missing.text());
    assert_eq!(missing.code(), "api.invalid_request");
    let too_long = "k".repeat(129);
    for key in ["has space", too_long.as_str(), ""] {
        let reply = raw(
            &f.router,
            "POST",
            HOST,
            path,
            Some("alice"),
            &[("idempotency-key", key)],
            Some(body.clone()),
        )
        .await;
        assert_eq!(reply.status, 400, "{key:?}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request");
    }
    let twice = raw(
        &f.router,
        "POST",
        HOST,
        path,
        Some("alice"),
        &[("idempotency-key", "a"), ("idempotency-key", "b")],
        Some(body),
    )
    .await;
    assert_eq!(twice.status, 400);
    assert_eq!(f.port.run_count(ACME), 0);
}

#[tokio::test]
async fn run_start_refuses_a_malformed_body_before_the_port() {
    let f = fixture();
    for (what, body) in [
        (
            "an actor",
            json!({"input": {"amount": "1"}, "starter": "bob"}),
        ),
        (
            "an actor named actor",
            json!({"input": {"amount": "1"}, "actor": "system:engine"}),
        ),
        ("a user", json!({"input": {"amount": "1"}, "user": "bob"})),
        ("no input", json!({"business_key": "x"})),
        ("input not an object", json!({"input": "a string"})),
        ("input an array", json!({"input": [1, 2]})),
        (
            "an empty business key",
            json!({"input": {}, "business_key": ""}),
        ),
        (
            "a business key with a space",
            json!({"input": {}, "business_key": "a b"}),
        ),
        (
            "a long business key",
            json!({"input": {}, "business_key": "k".repeat(129)}),
        ),
        (
            "a numeric business key",
            json!({"input": {}, "business_key": 5}),
        ),
        ("a body that is not an object", json!([1])),
    ] {
        let reply = start(&f, "alice", "expense_claim", "key-shape", &body).await;
        assert_eq!(reply.status, 400, "{what}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request", "{what}");
    }
    assert_eq!(f.port.run_count(ACME), 0);
    assert!(f.port.audit().is_empty(), "the port was never asked");
}

#[tokio::test]
async fn run_start_bounds_the_input() {
    let f = fixture();
    let mut many = serde_json::Map::new();
    for n in 0..201 {
        many.insert(format!("f{n}"), json!("x"));
    }
    let reply = start(
        &f,
        "alice",
        "expense_claim",
        "key-many",
        &json!({"input": Json::Object(many)}),
    )
    .await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    let big = "x".repeat(70 * 1024);
    let reply = start(
        &f,
        "alice",
        "expense_claim",
        "key-big",
        &json!({"input": {"amount": big}}),
    )
    .await;
    assert_eq!(reply.status, 400, "{}", reply.code());
    assert!(f.port.audit().is_empty());
}

#[tokio::test]
async fn run_start_refuses_a_body_over_the_cap_a_bad_media_type_and_bad_json() {
    let f = fixture();
    let path = "/api/v1/process/expense_claim/runs";
    let key = [("idempotency-key", "key-media")];
    let huge = vec![b' '; 200 * 1024];
    let reply = raw(
        &f.router,
        "POST",
        HOST,
        path,
        Some("alice"),
        &key,
        Some(huge),
    )
    .await;
    assert_eq!(reply.status, 413, "{}", reply.text());
    assert_eq!(reply.code(), "http.body_too_large");
    let reply = raw(
        &f.router,
        "POST",
        HOST,
        path,
        Some("alice"),
        &[
            ("idempotency-key", "key-media"),
            ("content-type", "text/plain"),
        ],
        Some(b"{}".to_vec()),
    )
    .await;
    assert_eq!(reply.status, 415, "{}", reply.text());
    assert_eq!(reply.code(), "api.unsupported_media_type");
    let reply = raw(
        &f.router,
        "POST",
        HOST,
        path,
        Some("alice"),
        &key,
        Some(b"{nope".to_vec()),
    )
    .await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    assert_eq!(reply.code(), "api.invalid_json");
    let reply = raw(&f.router, "POST", HOST, path, Some("alice"), &key, None).await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    assert_eq!(reply.code(), "api.invalid_json");
    assert_eq!(f.port.run_count(ACME), 0);
}

#[tokio::test]
async fn run_start_refuses_a_process_name_that_is_not_a_storage_name() {
    let f = fixture();
    for name in ["Bad", "1abc", "a-b", "x".repeat(64).as_str()] {
        let reply = start(&f, "alice", name, "key-name", &json!({"input": {}})).await;
        assert_eq!(reply.status, 400, "{name}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request");
    }
}

#[tokio::test]
async fn an_unknown_process_and_one_the_caller_may_not_start_are_the_same_answer() {
    let f = fixture();
    let body = json!({"input": {"period": "2026-10"}});
    let hidden = start(&f, "alice", "payroll_run", "key-h", &body).await;
    let unknown = start(&f, "alice", "no_such_process", "key-h2", &body).await;
    assert_eq!(hidden.status, 404, "{}", hidden.text());
    assert_eq!(hidden.code(), "process.not_found");
    assert_eq!(unknown.code(), "process.not_found");
    assert_eq!(
        strip(&hidden),
        strip(&unknown),
        "same status, body and headers"
    );
    assert_eq!(f.port.run_count(ACME), 0);
    // Both refusals were audited by the port: who, which process, which rule.
    let audit = f.port.audit();
    assert_eq!(audit.len(), 2);
    assert!(audit.iter().all(|a| a.operation == "run_start"
        && a.actor == "alice"
        && a.outcome == "process.not_found"));
}

#[tokio::test]
async fn invalid_input_is_422_with_field_paths_and_never_the_value() {
    let f = fixture();
    let secret = "SECRET-VALUE-123";
    let reply = start(
        &f,
        "alice",
        "expense_claim",
        "key-in",
        &json!({"input": {"extra": secret}}),
    )
    .await;
    assert_eq!(reply.status, 422, "{}", reply.text());
    assert_eq!(reply.code(), "process.input_invalid");
    assert!(
        !reply.text().contains(secret),
        "no submitted value is echoed"
    );
    let fields = reply.json()["error"]["fields"].as_array().unwrap().clone();
    let paths: Vec<&str> = fields
        .iter()
        .map(|f| f["field"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"amount"),
        "the missing field is named: {paths:?}"
    );
    assert!(
        paths.contains(&"extra"),
        "the unknown field is named: {paths:?}"
    );
    assert!(
        fields
            .iter()
            .all(|f| f["message"] == "This field is not valid.")
    );
    let audit = f.port.audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].outcome, "process.input_invalid");
}

#[tokio::test]
async fn the_starter_is_the_authenticated_caller_whatever_a_header_says() {
    let f = fixture();
    let id = start_with(
        &f,
        "alice",
        "expense_claim",
        "key-actor",
        &json!({"input": {"amount": "9"}}),
        &[
            ("x-actor", "bob"),
            ("x-user", "bob"),
            ("x-on-behalf-of", "bob"),
        ],
    )
    .await
    .run_id();
    assert!(!id.is_empty());
    // bob is neither the starter nor a party: the run is not his.
    let bob = get(&f, &format!("/api/v1/process/runs/{id}"), "bob").await;
    assert_eq!(bob.status, 404, "{}", bob.text());
    let alice = get(&f, &format!("/api/v1/process/runs/{id}"), "alice").await;
    assert_eq!(alice.status, 200);
    let events = get(&f, &format!("/api/v1/process/runs/{id}/events"), "alice").await;
    assert_eq!(events.json()["data"][0]["actor"], "alice");
}
