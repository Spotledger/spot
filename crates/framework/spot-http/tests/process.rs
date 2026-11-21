//! The process and run routes (`process_list`, `run_get`, `run_list`, `run_events` and
//! `run_cancel`, WP-BPM-02.T5) over [`MemoryProcessPort`]: every operation's success shape, the
//! list filters and paging, the visibility rule, every refusal code, and the two-site test. The
//! start (`run_start`, with its `Idempotency-Key`) is in `process_start.rs`; the fail-closed
//! behaviour with no port or a misbehaving one is in `process_fail_closed.rs`.
//!
//! The memory port decides nothing a production adapter decides differently: its user is the
//! credential's token itself, its processes and supervisors are what a test registers. The tests
//! prove the routes' half of the bargain: what is checked before the port, what is passed to it
//! and how its answer becomes the response.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::idempotency::{IdempotencyStore, MemoryIdempotencyStore};
use spot_http::api::ports::process::{MemoryProcessPort, ProcessDef, ProcessPort, RunState};
use spot_http::api::router::routes::{self, RouteState, SkipReason, process};
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
// process_list

#[tokio::test]
async fn process_list_shows_only_what_the_caller_may_start() {
    let f = fixture();
    let alice = get(&f, "/api/v1/process", "alice").await;
    assert_eq!(alice.status, 200, "{}", alice.text());
    assert_eq!(alice.header("cache-control"), Some("private, no-store"));
    assert!(alice.header("x-request-id").is_some());
    let body = alice.json();
    assert_eq!(body["next_cursor"], Json::Null);
    let names: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["expense_claim"],
        "payroll_run is absent for alice"
    );
    let item = &body["data"][0];
    assert_eq!(item["label"], "Expense claim");
    assert_eq!(item["version"], 1);
    assert_eq!(item["input_schema"]["type"], "object");
    assert_eq!(item["input_schema"]["additionalProperties"], false);

    let carol = get(&f, "/api/v1/process", "carol").await;
    let names: Vec<String> = carol.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, vec!["expense_claim", "payroll_run"]);
}

#[tokio::test]
async fn process_list_pages_with_an_opaque_cursor() {
    let f = fixture();
    let first = get(&f, "/api/v1/process?limit=1", "carol").await;
    assert_eq!(first.status, 200, "{}", first.text());
    let body = first.json();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    let cursor = body["next_cursor"].as_str().unwrap().to_owned();
    let second = get(
        &f,
        &format!("/api/v1/process?limit=1&cursor={cursor}"),
        "carol",
    )
    .await;
    assert_eq!(second.status, 200, "{}", second.text());
    assert_eq!(second.json()["data"][0]["name"], "payroll_run");
    assert_eq!(second.json()["next_cursor"], Json::Null);
}

#[tokio::test]
async fn process_list_refuses_a_bad_query_before_the_port() {
    let f = fixture();
    for query in [
        "limit=0",
        "limit=101",
        "limit=ten",
        "limit=-1",
        "limit=1&limit=2",
        "actor=bob",
        "user=bob",
        "site=beta",
        "cursor=",
        "cursor=bad%ZZ",
    ] {
        let reply = get(&f, &format!("/api/v1/process?{query}"), "alice").await;
        assert_eq!(reply.status, 400, "{query}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request", "{query}");
    }
    let long = "a".repeat(2049);
    let reply = get(&f, &format!("/api/v1/process?cursor={long}"), "alice").await;
    assert_eq!(reply.status, 400);
}

#[tokio::test]
async fn a_cursor_the_port_never_issued_is_a_bad_request() {
    let f = fixture();
    let reply = get(&f, "/api/v1/process?cursor=zzz", "alice").await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    assert_eq!(reply.code(), "api.invalid_request");
}

#[tokio::test]
async fn every_operation_needs_a_credential() {
    let f = fixture();
    let id = started(&f, "alice", "k-cred").await;
    let before = f.port.run_count(ACME);
    for (method, path) in [
        ("GET", "/api/v1/process".to_owned()),
        ("POST", "/api/v1/process/expense_claim/runs".to_owned()),
        ("GET", "/api/v1/process/runs".to_owned()),
        ("GET", format!("/api/v1/process/runs/{id}")),
        ("GET", format!("/api/v1/process/runs/{id}/events")),
        ("POST", format!("/api/v1/process/runs/{id}/cancel")),
    ] {
        let body = (method == "POST").then(|| b"{}".to_vec());
        let reply = raw(
            &f.router,
            method,
            HOST,
            &path,
            None,
            &[("idempotency-key", "k-none")],
            body,
        )
        .await;
        assert_eq!(reply.status, 401, "{path}: {}", reply.text());
        assert_eq!(reply.code(), "auth.required", "{path}");
        assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    }
    assert_eq!(f.port.run_count(ACME), before, "nothing was started");
    assert_eq!(f.port.audit().len(), 1, "the port was never asked");
}

// ------------------------------------------------------------------------------------------
// run_get

#[tokio::test]
async fn run_get_follows_the_visibility_rule() {
    let f = fixture();
    let id = started(&f, "alice", "key-get").await;
    let path = format!("/api/v1/process/runs/{id}");
    let own = get(&f, &path, "alice").await;
    assert_eq!(own.status, 200, "{}", own.text());
    assert_eq!(own.header("cache-control"), Some("private, no-store"));
    assert_eq!(own.json()["data"]["id"], id.as_str());
    // A supervisor sees every run; an assignee sees theirs.
    assert_eq!(get(&f, &path, "sue").await.status, 200);
    f.port.add_party(ACME, &id, "dave");
    assert_eq!(get(&f, &path, "dave").await.status, 200);
    // A stranger gets the one answer a run that does not exist gets.
    let stranger = get(&f, &path, "bob").await;
    let missing = get(&f, "/api/v1/process/runs/run_99999999999999999999", "alice").await;
    assert_eq!(stranger.status, 404, "{}", stranger.text());
    assert_eq!(stranger.code(), "process.run_not_found");
    assert_eq!(
        strip(&stranger),
        strip(&missing),
        "byte-identical but the request id"
    );
}

#[tokio::test]
async fn a_failed_run_carries_a_failure_code_and_no_error_text() {
    let f = fixture();
    let id = started(&f, "alice", "key-failed").await;
    f.port.force_state(ACME, &id, RunState::Failed);
    let reply = get(&f, &format!("/api/v1/process/runs/{id}"), "alice").await;
    let run = reply.json()["data"].clone();
    assert_eq!(run["state"], "failed");
    assert_eq!(run["failure_code"], "bpm.node_failed");
    assert!(run["ended_at"].is_string());
    assert!(run.get("error").is_none() && run.get("message").is_none());
}

#[tokio::test]
async fn run_get_refuses_an_id_that_is_not_in_the_documented_form() {
    let f = fixture();
    for id in [
        "short",
        "has%20space0123456789",
        "dots.dots.dots.dots",
        "a".repeat(65).as_str(),
    ] {
        let reply = get(&f, &format!("/api/v1/process/runs/{id}"), "alice").await;
        assert_eq!(reply.status, 400, "{id}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request");
    }
    let reply = get(
        &f,
        "/api/v1/process/runs/run_00000000000000000001?x=1",
        "alice",
    )
    .await;
    assert_eq!(reply.status, 400, "a query is refused: {}", reply.text());
}

// ------------------------------------------------------------------------------------------
// run_list

#[tokio::test]
async fn run_list_is_newest_first_and_filtered_by_visibility_before_paging() {
    let f = fixture();
    let a1 = started(&f, "alice", "l-a1").await;
    let b1 = started(&f, "bob", "l-b1").await;
    let a2 = started(&f, "alice", "l-a2").await;
    let alice = get(&f, "/api/v1/process/runs", "alice").await;
    assert_eq!(alice.status, 200, "{}", alice.text());
    assert_eq!(alice.header("cache-control"), Some("private, no-store"));
    let ids: Vec<String> = alice.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        vec![a2.clone(), a1.clone()],
        "newest first, bob's run absent"
    );
    assert_eq!(alice.json()["next_cursor"], Json::Null);
    assert!(alice.json().get("total").is_none(), "no total is returned");
    // A page of one still never counts or names bob's run.
    let page = get(&f, "/api/v1/process/runs?limit=1", "alice").await;
    assert_eq!(page.json()["data"][0]["id"], a2.as_str());
    let cursor = page.json()["next_cursor"].as_str().unwrap().to_owned();
    let next = get(
        &f,
        &format!("/api/v1/process/runs?limit=1&cursor={cursor}"),
        "alice",
    )
    .await;
    assert_eq!(next.json()["data"][0]["id"], a1.as_str());
    assert_eq!(next.json()["next_cursor"], Json::Null);
    // A supervisor sees all three; bob sees his own.
    let sue = get(&f, "/api/v1/process/runs", "sue").await;
    assert_eq!(sue.json()["data"].as_array().unwrap().len(), 3);
    let bob = get(&f, "/api/v1/process/runs", "bob").await;
    assert_eq!(bob.json()["data"][0]["id"], b1.as_str());
    assert_eq!(bob.json()["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn run_list_filters_by_process_state_and_mine() {
    let f = fixture();
    let own = started(&f, "sue", "f-own").await;
    let other = started(&f, "alice", "f-other").await;
    f.port.force_state(ACME, &other, RunState::Completed);
    let ids = |reply: &Reply| -> Vec<String> {
        reply.json()["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let mine = get(&f, "/api/v1/process/runs?mine=true", "sue").await;
    assert_eq!(ids(&mine), vec![own.clone()]);
    let completed = get(&f, "/api/v1/process/runs?state=completed", "sue").await;
    assert_eq!(ids(&completed), vec![other.clone()]);
    let running = get(
        &f,
        "/api/v1/process/runs?state=running&process=expense_claim",
        "sue",
    )
    .await;
    assert_eq!(ids(&running), vec![own.clone()]);
    // A filter that matches nothing the caller may see is an empty page, never an error.
    let none = get(&f, "/api/v1/process/runs?process=payroll_run", "sue").await;
    assert_eq!(none.status, 200);
    assert_eq!(none.json()["data"], json!([]));
    let not_mine = get(&f, "/api/v1/process/runs?mine=false&state=cancelled", "sue").await;
    assert_eq!(not_mine.json()["data"], json!([]));
}

#[tokio::test]
async fn run_list_refuses_a_bad_query_before_the_port() {
    let f = fixture();
    for query in [
        "state=finished",
        "state=",
        "mine=maybe",
        "mine=1",
        "process=Bad-Name",
        "limit=0",
        "limit=101",
        "starter=bob",
        "actor=bob",
        "state=running&state=failed",
    ] {
        let reply = get(&f, &format!("/api/v1/process/runs?{query}"), "alice").await;
        assert_eq!(reply.status, 400, "{query}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request", "{query}");
    }
}

// ------------------------------------------------------------------------------------------
// run_events

#[tokio::test]
async fn run_events_pages_by_seq_without_a_payload() {
    let f = fixture();
    let id = started(&f, "alice", "e-1").await;
    let c = cancel(
        &f,
        "alice",
        &id,
        &json!({"reason": "Entered for the wrong customer."}),
    )
    .await;
    assert_eq!(c.status, 200, "{}", c.text());
    let path = format!("/api/v1/process/runs/{id}/events");
    let all = get(&f, &path, "alice").await;
    assert_eq!(all.status, 200, "{}", all.text());
    assert_eq!(all.header("cache-control"), Some("private, no-store"));
    let body = all.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["seq"], 1);
    assert_eq!(data[0]["kind"], "run.started");
    assert_eq!(data[0]["actor"], "alice");
    assert_eq!(data[1]["seq"], 2);
    assert_eq!(data[1]["kind"], "run.cancelled");
    assert_eq!(body["next_after"], 2);
    for event in data {
        assert!(event.get("payload").is_none() && event.get("data").is_none());
    }
    assert!(
        !all.text().contains("wrong customer"),
        "the reason is audit-only"
    );
    // `after` and `limit`.
    let first = get(&f, &format!("{path}?limit=1"), "alice").await;
    assert_eq!(first.json()["data"].as_array().unwrap().len(), 1);
    assert_eq!(first.json()["next_after"], 1);
    let rest = get(&f, &format!("{path}?after=1"), "alice").await;
    assert_eq!(rest.json()["data"][0]["seq"], 2);
    // An empty page has no `next_after`.
    let empty = get(&f, &format!("{path}?after=2"), "alice").await;
    assert_eq!(empty.status, 200);
    assert_eq!(empty.json()["data"], json!([]));
    assert!(empty.json().get("next_after").is_none());
}

#[tokio::test]
async fn run_events_has_the_visibility_and_refusals_of_run_get() {
    let f = fixture();
    let id = started(&f, "alice", "e-2").await;
    let path = format!("/api/v1/process/runs/{id}/events");
    let stranger = get(&f, &path, "bob").await;
    let missing = get(
        &f,
        "/api/v1/process/runs/run_99999999999999999999/events",
        "bob",
    )
    .await;
    assert_eq!(stranger.status, 404);
    assert_eq!(stranger.code(), "process.run_not_found");
    assert_eq!(strip(&stranger), strip(&missing));
    assert_eq!(get(&f, &path, "sue").await.status, 200);
    for query in [
        "after=-1",
        "after=x",
        "after=",
        "limit=0",
        "limit=201",
        "starter=bob",
    ] {
        let reply = get(&f, &format!("{path}?{query}"), "alice").await;
        assert_eq!(reply.status, 400, "{query}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request", "{query}");
    }
    let reply = get(&f, "/api/v1/process/runs/short/events", "alice").await;
    assert_eq!(reply.status, 400);
}

// ------------------------------------------------------------------------------------------
// run_cancel

#[tokio::test]
async fn the_starter_cancels_a_run_and_the_run_is_returned_in_its_new_state() {
    let f = fixture();
    let id = started(&f, "alice", "c-1").await;
    let reply = cancel(&f, "alice", &id, &json!({"reason": "Entered twice."})).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    let run = reply.json()["data"].clone();
    assert_eq!(run["id"], id.as_str());
    assert_eq!(run["state"], "cancelled");
    assert!(run["ended_at"].is_string());
    assert_eq!(run["steps"][0]["state"], "cancelled");
    assert!(
        !reply.text().contains("Entered twice."),
        "the reason is not echoed"
    );
    // A repeat is `process.run_not_active`.
    let again = cancel(&f, "alice", &id, &json!({"reason": "Again."})).await;
    assert_eq!(again.status, 409, "{}", again.text());
    assert_eq!(again.code(), "process.run_not_active");
}

#[tokio::test]
async fn cancel_is_for_the_starter_and_a_supervisor_and_refusals_are_audited() {
    let f = fixture();
    let id = started(&f, "alice", "c-2").await;
    f.port.add_party(ACME, &id, "dave");
    // dave may see the run but may not cancel it: the caller knows the run exists.
    let denied = cancel(&f, "dave", &id, &json!({"reason": "Not mine."})).await;
    assert_eq!(denied.status, 403, "{}", denied.text());
    assert_eq!(denied.code(), "permission.denied");
    // bob may not see it at all.
    let hidden = cancel(&f, "bob", &id, &json!({"reason": "Not mine."})).await;
    let missing = cancel(
        &f,
        "bob",
        "run_99999999999999999999",
        &json!({"reason": "Not mine."}),
    )
    .await;
    assert_eq!(hidden.status, 404, "{}", hidden.text());
    assert_eq!(hidden.code(), "process.run_not_found");
    assert_eq!(strip(&hidden), strip(&missing));
    // The run is still running; a supervisor may cancel it.
    assert_eq!(
        get(&f, &format!("/api/v1/process/runs/{id}"), "alice")
            .await
            .json()["data"]["state"],
        "running"
    );
    let sue = cancel(&f, "sue", &id, &json!({"reason": "Supervisor stop."})).await;
    assert_eq!(sue.status, 200, "{}", sue.text());
    let audit = f.port.audit();
    let cancels: Vec<(&str, &str)> = audit
        .iter()
        .filter(|a| a.operation == "run_cancel")
        .map(|a| (a.actor.as_str(), a.outcome))
        .collect();
    assert_eq!(
        cancels,
        vec![
            ("dave", "permission.denied"),
            ("bob", "process.run_not_found"),
            ("bob", "process.run_not_found"),
            ("sue", "ok"),
        ]
    );
}

#[tokio::test]
async fn a_run_that_has_ended_cannot_be_cancelled() {
    let f = fixture();
    let id = started(&f, "alice", "c-3").await;
    f.port.force_state(ACME, &id, RunState::Completed);
    let reply = cancel(&f, "alice", &id, &json!({"reason": "Too late."})).await;
    assert_eq!(reply.status, 409, "{}", reply.text());
    assert_eq!(reply.code(), "process.run_not_active");
}

#[tokio::test]
async fn run_cancel_refuses_a_malformed_body_before_the_port() {
    let f = fixture();
    let id = started(&f, "alice", "c-4").await;
    let audited = f.port.audit().len();
    for (what, body) in [
        ("no reason", json!({})),
        ("an empty reason", json!({"reason": ""})),
        ("a long reason", json!({"reason": "r".repeat(501)})),
        ("a numeric reason", json!({"reason": 5})),
        ("an actor", json!({"reason": "x", "actor": "system:engine"})),
        ("a user", json!({"reason": "x", "user": "bob"})),
        ("an array", json!(["x"])),
    ] {
        let reply = cancel(&f, "alice", &id, &body).await;
        assert_eq!(reply.status, 400, "{what}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request", "{what}");
    }
    let path = format!("/api/v1/process/runs/{id}/cancel");
    let reply = raw(&f.router, "POST", HOST, &path, Some("alice"), &[], None).await;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.code(), "api.invalid_json");
    let reply = raw(
        &f.router,
        "POST",
        HOST,
        &path,
        Some("alice"),
        &[("content-type", "text/plain")],
        Some(b"{}".to_vec()),
    )
    .await;
    assert_eq!(reply.status, 415);
    let reply = raw(
        &f.router,
        "POST",
        HOST,
        &path,
        Some("alice"),
        &[],
        Some(vec![b' '; 40 * 1024]),
    )
    .await;
    assert_eq!(reply.status, 413);
    assert_eq!(f.port.audit().len(), audited, "the port was never asked");
    // A reason of exactly 500 multi-byte characters is accepted (the bound is characters).
    let reply = cancel(&f, "alice", &id, &json!({"reason": "é".repeat(500)})).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
}

// ------------------------------------------------------------------------------------------
// Two sites

#[tokio::test]
async fn one_sites_token_reads_nothing_of_the_other_site() {
    let f = fixture();
    let id = started(&f, "alice", "s-1").await;
    // The same token on the other site: no such run, an empty list, and no cancel.
    let get_b = get_on(&f, HOST_B, &format!("/api/v1/process/runs/{id}"), "alice").await;
    assert_eq!(get_b.status, 404, "{}", get_b.text());
    assert_eq!(get_b.code(), "process.run_not_found");
    let events_b = get_on(
        &f,
        HOST_B,
        &format!("/api/v1/process/runs/{id}/events"),
        "alice",
    )
    .await;
    assert_eq!(events_b.status, 404);
    let list_b = get_on(&f, HOST_B, "/api/v1/process/runs", "alice").await;
    assert_eq!(list_b.json()["data"], json!([]));
    let cancel_b = raw(
        &f.router,
        "POST",
        HOST_B,
        &format!("/api/v1/process/runs/{id}/cancel"),
        Some("alice"),
        &[],
        Some(b"{\"reason\":\"x\"}".to_vec()),
    )
    .await;
    assert_eq!(cancel_b.status, 404);
    // Each site lists its own processes: payroll_run exists on acme only.
    let procs_b = get_on(&f, HOST_B, "/api/v1/process", "carol").await;
    let names: Vec<String> = procs_b.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, vec!["expense_claim"]);
    // A supervisor of acme supervises nothing on beta.
    let sue_b = get_on(&f, HOST_B, &format!("/api/v1/process/runs/{id}"), "sue").await;
    assert_eq!(sue_b.status, 404);
    // And acme's run is untouched.
    let acme = get(&f, &format!("/api/v1/process/runs/{id}"), "alice").await;
    assert_eq!(acme.json()["data"]["state"], "running");
    // No parameter names a tenant.
    let reply = get(&f, "/api/v1/process/runs?site=beta", "alice").await;
    assert_eq!(reply.status, 400);
}

// ------------------------------------------------------------------------------------------
// The group

#[tokio::test]
async fn only_the_documented_methods_are_served() {
    let f = fixture();
    for (method, path) in [
        ("DELETE", "/api/v1/process"),
        ("PUT", "/api/v1/process/runs"),
        ("POST", "/api/v1/process/runs/run_00000000000000000001"),
        ("GET", "/api/v1/process/expense_claim/runs"),
        (
            "GET",
            "/api/v1/process/runs/run_00000000000000000001/cancel",
        ),
    ] {
        let reply = raw(&f.router, method, HOST, path, Some("alice"), &[], None).await;
        assert_eq!(reply.status, 405, "{method} {path}");
    }
    assert!(f.store.live_entries() == 0, "no key was reserved");
}

#[test]
fn the_group_declares_its_operations_and_needs_both_ports() {
    assert_eq!(
        process::SERVES,
        &[
            "process_list",
            "run_start",
            "run_list",
            "run_get",
            "run_events",
            "run_cancel"
        ]
    );
    assert_eq!(process::required_ports().len(), 2);
    let registration = process::registration();
    assert_eq!(registration.name, "process");
    assert!(registration.public.is_none(), "the group is authenticated");
}

#[test]
fn every_served_id_is_an_operation_of_the_contract() {
    let contract = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    ))
    .unwrap();
    for id in process::SERVES {
        assert!(
            contract
                .lines()
                .any(|line| line.trim() == format!("operationId: {id}")),
            "{id} is not an operationId of the contract"
        );
    }
}

#[test]
fn the_registry_mounts_the_group_only_with_both_ports() {
    let port = Arc::new(MemoryProcessPort::new());
    let mut only_port = Ports::new();
    only_port.provide::<dyn ProcessPort>(port.clone());
    let mounted = routes::mount(&[process::registration()], &only_port, &[]);
    assert!(mounted.report.mounted.is_empty());
    assert!(matches!(
        mounted.report.skipped.as_slice(),
        [("process", SkipReason::MissingPort(_))]
    ));
    let mut both = only_port.clone();
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    both.provide::<dyn IdempotencyStore>(Arc::new(MemoryIdempotencyStore::new(clock)));
    let mounted = routes::mount(&[process::registration()], &both, &[]);
    assert_eq!(mounted.report.mounted, vec!["process"]);
    assert!(mounted.report.served.contains("run_start"));
    assert!(
        mounted.report.public.is_empty(),
        "no process route is public here"
    );
}
