//! Fail closed (WP-BPM-02.T5): the process routes with no port attached (`501
//! service.not_configured`, never `200` with an empty list), with no idempotency store (a run is
//! never started unreserved), with a port that refuses with each declared error (rendered with
//! its code and nothing more) and with a port whose answers break the contract's bounds (`500
//! http.internal`, no detail).
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::idempotency::{IdempotencyStore, MemoryIdempotencyStore};
use spot_http::api::ports::process::{
    DocumentRef, ListPage, MemoryProcessPort, ProcessDef, ProcessPort, ProcessPortError,
    ProcessResult, ProcessSummary, Run, RunEvent, RunQuery, RunStart, RunState, RunStep, StepState,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{RouteState, process};
use spot_http::api::service::{BoxFuture, RequestCtx};
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
// Fail closed

fn router_with_ports(ports: &Ports) -> axum::Router {
    // The state holds exactly what the test provides (a host mounting the routes itself).
    ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(process::routes().with_state(RouteState::new(ports.clone())))
        .build()
}

#[tokio::test]
async fn with_no_port_every_operation_answers_501_never_an_empty_list() {
    let router = router_with_ports(&Ports::new());
    let id = "run_00000000000000000001";
    let calls = [
        ("GET", "/api/v1/process".to_owned(), false),
        (
            "POST",
            "/api/v1/process/expense_claim/runs".to_owned(),
            true,
        ),
        ("GET", "/api/v1/process/runs".to_owned(), false),
        ("GET", format!("/api/v1/process/runs/{id}"), false),
        ("GET", format!("/api/v1/process/runs/{id}/events"), false),
        ("POST", format!("/api/v1/process/runs/{id}/cancel"), true),
    ];
    for (method, path, has_body) in calls {
        let body = has_body.then(|| b"{\"input\":{},\"reason\":\"x\"}".to_vec());
        let reply = raw(
            &router,
            method,
            HOST,
            &path,
            Some("alice"),
            &[("idempotency-key", "k")],
            body,
        )
        .await;
        assert_eq!(reply.status, 501, "{path}: {}", reply.text());
        assert_eq!(reply.code(), "service.not_configured", "{path}");
        assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    }
}

#[tokio::test]
async fn a_start_without_the_idempotency_store_is_501_and_starts_nothing() {
    let port = Arc::new(MemoryProcessPort::new());
    port.define(
        ACME,
        ProcessDef::open("expense_claim", "Expense claim", &["amount"]),
    );
    let mut ports = Ports::new();
    ports.provide::<dyn ProcessPort>(port.clone());
    let router = router_with_ports(&ports);
    let reply = raw(
        &router,
        "POST",
        HOST,
        "/api/v1/process/expense_claim/runs",
        Some("alice"),
        &[("idempotency-key", "k")],
        Some(b"{\"input\":{\"amount\":\"1\"}}".to_vec()),
    )
    .await;
    assert_eq!(reply.status, 501, "{}", reply.text());
    assert_eq!(port.run_count(ACME), 0, "a run is never started unreserved");
}

/// A port that refuses every call with one error.
struct Failing(ProcessPortError);

fn failing<T: Send + 'static>(error: &ProcessPortError) -> BoxFuture<'static, ProcessResult<T>> {
    let error = error.clone();
    Box::pin(async move { Err(error) })
}

impl ProcessPort for Failing {
    fn list_processes<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: Option<&'a str>,
        _: u32,
    ) -> BoxFuture<'a, ProcessResult<ListPage<ProcessSummary>>> {
        failing(&self.0)
    }
    fn start_run<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RunStart,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        failing(&self.0)
    }
    fn list_runs<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: RunQuery,
    ) -> BoxFuture<'a, ProcessResult<ListPage<Run>>> {
        failing(&self.0)
    }
    fn get_run<'a>(&'a self, _: &'a RequestCtx, _: &'a str) -> BoxFuture<'a, ProcessResult<Run>> {
        failing(&self.0)
    }
    fn run_events<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: u64,
        _: u32,
    ) -> BoxFuture<'a, ProcessResult<Vec<RunEvent>>> {
        failing(&self.0)
    }
    fn cancel_run<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        failing(&self.0)
    }
}

fn router_over_port(port: Arc<dyn ProcessPort>) -> axum::Router {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let mut ports = Ports::new();
    ports.provide::<dyn ProcessPort>(port);
    ports.provide::<dyn IdempotencyStore>(Arc::new(MemoryIdempotencyStore::new(clock)));
    router_over(&ports)
}

/// Every operation, as a request, for the port-misbehaviour tests.
fn every_call() -> Vec<(&'static str, String, Option<Vec<u8>>)> {
    let id = "run_00000000000000000001";
    vec![
        ("GET", "/api/v1/process".to_owned(), None),
        (
            "POST",
            "/api/v1/process/expense_claim/runs".to_owned(),
            Some(b"{\"input\":{\"amount\":\"1\"}}".to_vec()),
        ),
        ("GET", "/api/v1/process/runs".to_owned(), None),
        ("GET", format!("/api/v1/process/runs/{id}"), None),
        ("GET", format!("/api/v1/process/runs/{id}/events"), None),
        (
            "POST",
            format!("/api/v1/process/runs/{id}/cancel"),
            Some(b"{\"reason\":\"x\"}".to_vec()),
        ),
    ]
}

#[tokio::test]
async fn a_port_error_is_rendered_with_its_declared_code_and_nothing_more() {
    for (error, status, code) in [
        (ProcessPortError::Internal, 500, "http.internal"),
        (ProcessPortError::Unavailable, 503, "server.unavailable"),
        (ProcessPortError::NotFound, 404, "process.not_found"),
        (ProcessPortError::RunNotFound, 404, "process.run_not_found"),
        (
            ProcessPortError::RunNotActive,
            409,
            "process.run_not_active",
        ),
        (ProcessPortError::PermissionDenied, 403, "permission.denied"),
        (ProcessPortError::BadCursor, 400, "api.invalid_request"),
    ] {
        let router = router_over_port(Arc::new(Failing(error.clone())));
        for (method, path, body) in every_call() {
            let reply = raw(
                &router,
                method,
                HOST,
                &path,
                Some("alice"),
                &[("idempotency-key", "k-err")],
                body,
            )
            .await;
            assert_eq!(reply.status, status, "{error:?} {path}: {}", reply.text());
            assert_eq!(reply.code(), code, "{error:?} {path}");
            if error == ProcessPortError::Internal {
                let body = reply.json();
                assert!(body["error"].get("fields").is_none());
                assert!(body["error"].get("details").is_none());
                assert_eq!(
                    body["error"]["message"],
                    Code::HttpInternal.default_message()
                );
            }
        }
    }
}

#[tokio::test]
async fn a_port_input_issue_keeps_the_path_and_code_and_loses_the_message() {
    use spot_http::api::service::FieldIssue;
    let issue = FieldIssue::new(
        "amount",
        "process.input_invalid_format",
        "Bad value 'SECRET-123'",
    );
    let router = router_over_port(Arc::new(Failing(ProcessPortError::InputInvalid(vec![
        issue,
    ]))));
    let reply = raw(
        &router,
        "POST",
        HOST,
        "/api/v1/process/expense_claim/runs",
        Some("alice"),
        &[("idempotency-key", "k-issue")],
        Some(b"{\"input\":{\"amount\":\"SECRET-123\"}}".to_vec()),
    )
    .await;
    assert_eq!(reply.status, 422, "{}", reply.text());
    assert!(!reply.text().contains("SECRET-123"));
    assert_eq!(reply.json()["error"]["fields"][0]["field"], "amount");
    assert_eq!(
        reply.json()["error"]["fields"][0]["code"],
        "process.input_invalid_format"
    );
}

/// A port whose answers break the contract's bounds.
struct Greedy;

fn wide_run() -> Run {
    Run {
        id: "run_00000000000000000001".to_owned(),
        process: "expense_claim".to_owned(),
        version: 1,
        state: RunState::Running,
        business_key: None,
        started_at: "2026-11-01T00:00:00Z".to_owned(),
        ended_at: None,
        failure_code: None,
        steps: (0..501)
            .map(|n| RunStep {
                node: format!("n{n}"),
                label: "Step".to_owned(),
                state: StepState::Pending,
                started_at: None,
                ended_at: None,
            })
            .collect(),
        documents: vec![DocumentRef {
            doctype: "expense".to_owned(),
            id: "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f".to_owned(),
        }],
    }
}

impl ProcessPort for Greedy {
    fn list_processes<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, ProcessResult<ListPage<ProcessSummary>>> {
        Box::pin(async move {
            let item = ProcessSummary {
                name: "p".to_owned(),
                label: "P".to_owned(),
                version: 1,
                input_schema: json!({}),
            };
            Ok(ListPage {
                items: vec![item; limit as usize + 1],
                next_cursor: None,
            })
        })
    }
    fn start_run<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RunStart,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        Box::pin(async { Ok(wide_run()) })
    }
    fn list_runs<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: RunQuery,
    ) -> BoxFuture<'a, ProcessResult<ListPage<Run>>> {
        Box::pin(async {
            Ok(ListPage {
                items: vec![wide_run()],
                next_cursor: None,
            })
        })
    }
    fn get_run<'a>(&'a self, _: &'a RequestCtx, _: &'a str) -> BoxFuture<'a, ProcessResult<Run>> {
        Box::pin(async { Ok(wide_run()) })
    }
    fn run_events<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: u64,
        _: u32,
    ) -> BoxFuture<'a, ProcessResult<Vec<RunEvent>>> {
        Box::pin(async {
            let event = |seq: u64| RunEvent {
                seq,
                at: "2026-11-01T00:00:00Z".to_owned(),
                kind: "run.started".to_owned(),
                node: None,
                actor: None,
            };
            // Out of order: seq must rise.
            Ok(vec![event(2), event(1)])
        })
    }
    fn cancel_run<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, ProcessResult<Run>> {
        Box::pin(async { Ok(wide_run()) })
    }
}

#[tokio::test]
async fn a_port_answer_outside_the_contract_is_a_500_with_no_detail() {
    let router = router_over_port(Arc::new(Greedy));
    for (method, path, body) in every_call() {
        let reply = raw(
            &router,
            method,
            HOST,
            &path,
            Some("alice"),
            &[("idempotency-key", "k-greedy")],
            body,
        )
        .await;
        assert_eq!(reply.status, 500, "{path}: {}", reply.text());
        assert_eq!(reply.code(), "http.internal", "{path}");
        assert!(reply.json()["error"].get("details").is_none());
    }
}
