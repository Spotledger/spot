//! The connector routes (`connector_list`, `connector_get`, `connector_invoke`,
//! `connector_runs`) and the unauthenticated ingress (`connector_inbound`), WP-INT-05.T4, over
//! [`MemoryConnectorsPort`]. What the tests prove is the routes' half of the bargain: what is
//! checked before the port, what is passed to it, how its answer becomes the response, and, for
//! the ingress, that the signature check runs in constant time before any parse, that every
//! refusal is one answer, that nothing secret is echoed and that the limits and caps hold.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[allow(dead_code)]
mod support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use hyper::body::Frame;
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::ports::connectors::{
    ConnectorSeed, ConnectorState, ConnectorsPort, InboundCredential, MemoryConnectorsPort,
    RunOutcome, RunView,
};
use spot_http::api::router::routes::connectors::{self, inbound};
use spot_http::api::router::routes::{RateClass, RouteState};
use spot_http::api::router::{ApiServices, router_with};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use support::fake::FakeApp;
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const HOST_B: &str = "erp.beta.com";
const ACME: &str = "acme";
const BETA: &str = "beta";
const NOW: i64 = 1_800_000_000;
const SECRET: &[u8] = b"hunter2-ingress-secret";
const BEARER: &[u8] = b"conn-bearer-token-9f31";
const OVERSIZE: usize = inbound::MAX_BODY_BYTES;
const DELIVERY_BODY: &[u8] = br#"{"event":"order.created","order":"SO-77","amount":"120.50"}"#;

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
}

/// The answer without the per-request id, for comparing two answers.
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

fn sites() -> StaticSites {
    let acme = SiteId::new(ACME).unwrap();
    let beta = SiteId::new(BETA).unwrap();
    StaticSites::new()
        .add(HOST, acme.clone())
        .add(HOST_B, beta.clone())
        .add_site_name(acme)
        .add_site_name(beta)
}

fn state_over(port: &Arc<MemoryConnectorsPort>) -> RouteState {
    let mut ports = Ports::new();
    ports.provide::<dyn ConnectorsPort>(port.clone());
    RouteState::new(ports.subset(connectors::required_ports()).unwrap())
}

fn mount(routes: axum::Router<RouteState>, state: RouteState) -> axum::Router {
    ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(routes.with_state(state))
        .build()
}

fn api_router(port: &Arc<MemoryConnectorsPort>) -> axum::Router {
    mount(connectors::routes(), state_over(port))
}

fn inbound_router(port: &Arc<MemoryConnectorsPort>) -> axum::Router {
    mount(inbound::routes(), state_over(port))
}

fn bare_router() -> axum::Router {
    mount(connectors::routes(), RouteState::new(Ports::new()))
}

fn run(at: &str, operation: &str, outcome: RunOutcome, code: Option<&str>) -> RunView {
    RunView {
        at: at.to_owned(),
        operation: operation.to_owned(),
        outcome,
        duration_ms: 42,
        code: code.map(str::to_owned),
    }
}

/// The fixture: acme has `crm` (HMAC ingress, a long `export`, `echo` needs `customer`),
/// `bearer_hook` (bearer ingress), `legacy` (disabled) and `erp_sync`; beta has its own `crm`.
/// alice sees and invokes everything and reads run logs, bob sees only `erp_sync`, carol sees
/// everything but may not invoke.
fn fixture() -> Arc<MemoryConnectorsPort> {
    let port = Arc::new(MemoryConnectorsPort::new());
    let mut crm = ConnectorSeed::new("crm");
    crm.view
        .operations
        .push(spot_http::api::ports::connectors::OperationView {
            name: "export".to_owned(),
            description: None,
            input_schema: serde_json::Map::new(),
            output_schema: serde_json::Map::new(),
        });
    crm.long_operations = vec!["export".to_owned()];
    crm.required_input
        .insert("echo".to_owned(), vec!["customer".to_owned()]);
    crm.output = json!({"echoed": true}).as_object().unwrap().clone();
    crm.inbound = Some(InboundCredential::Hmac(SECRET.to_vec()));
    port.add_connector(ACME, crm);
    let mut hook = ConnectorSeed::new("bearer_hook");
    hook.inbound = Some(InboundCredential::Bearer(BEARER.to_vec()));
    port.add_connector(ACME, hook);
    let mut legacy = ConnectorSeed::new("legacy");
    legacy.view.state = ConnectorState::Disabled;
    legacy.inbound = Some(InboundCredential::Hmac(SECRET.to_vec()));
    port.add_connector(ACME, legacy);
    port.add_connector(ACME, ConnectorSeed::new("erp_sync"));
    let mut beta_crm = ConnectorSeed::new("crm");
    beta_crm.inbound = Some(InboundCredential::Hmac(b"beta-secret".to_vec()));
    port.add_connector(BETA, beta_crm);
    for (token, user) in [
        ("tok-alice", "alice"),
        ("tok-bob", "bob"),
        ("tok-carol", "carol"),
    ] {
        port.grant(ACME, token, user);
    }
    port.grant(BETA, "tok-alice", "alice");
    port.restrict(ACME, "tok-bob", &["erp_sync"]);
    port.revoke_invoke(ACME, "tok-carol");
    port.grant_runs(ACME, "tok-alice");
    let runs = [
        ("09", "echo", RunOutcome::Ok, None),
        (
            "10",
            "export",
            RunOutcome::Failed,
            Some("connector.upstream_failed"),
        ),
        ("11", "echo", RunOutcome::Ok, None),
    ];
    for (hour, operation, outcome, code) in runs {
        let at = format!("2026-11-12T{hour}:00:00Z");
        port.add_run(ACME, "crm", run(&at, operation, outcome, code));
    }
    port
}

async fn finish(response: axum::response::Response) -> Reply {
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

async fn call(
    router: &axum::Router,
    method: &str,
    host: &str,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<Json>,
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
    let payload = match body {
        Some(json) => {
            if !has_content_type {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
            }
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    finish(
        router
            .clone()
            .oneshot(builder.body(payload).unwrap())
            .await
            .unwrap(),
    )
    .await
}

/// Asserts the status and, when `code` is not empty, the error code.
fn expect(reply: &Reply, status: u16, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.text());
    if !code.is_empty() {
        assert_eq!(reply.code(), code, "{}", reply.text());
    }
}

async fn get(router: &axum::Router, path: &str, token: &str) -> Reply {
    call(router, "GET", HOST, path, Some(token), &[], None).await
}

async fn invoke(
    router: &axum::Router,
    path: &str,
    token: &str,
    key: Option<&str>,
    body: Json,
) -> Reply {
    let extra: Vec<(&str, &str)> = key.map(|k| ("idempotency-key", k)).into_iter().collect();
    call(router, "POST", HOST, path, Some(token), &extra, Some(body)).await
}

// connector_list and connector_get

async fn names(router: &axum::Router, path: &str, token: &str) -> Vec<String> {
    let reply = get(router, path, token).await;
    expect(&reply, 200, "");
    let data = reply.json()["data"].clone();
    data.as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_list_is_the_contract_shape_pages_and_hides_what_the_caller_may_not_see() {
    let router = api_router(&fixture());
    let reply = get(&router, "/api/v1/connectors?limit=2", "tok-alice").await;
    expect(&reply, 200, "");
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    let body = reply.json();
    assert_eq!(body["data"][1]["name"], "crm");
    assert_eq!(body["data"][1]["state"], "enabled");
    assert_eq!(body["data"][1]["operations"][0]["name"], "echo");
    let cursor = body["next_cursor"].as_str().unwrap();
    let rest_path = format!("/api/v1/connectors?limit=2&cursor={cursor}");
    let rest = names(&router, &rest_path, "tok-alice");
    assert_eq!(rest.await, ["erp_sync", "legacy"]);
    assert_eq!(
        names(&router, "/api/v1/connectors", "tok-bob").await,
        ["erp_sync"]
    );
}

#[tokio::test]
async fn get_answers_one_connector_and_a_hidden_or_foreign_one_is_the_one_404() {
    let router = api_router(&fixture());
    let ok = get(&router, "/api/v1/connectors/crm", "tok-alice").await;
    expect(&ok, 200, "");
    assert_eq!(ok.json()["data"]["name"], "crm");
    let missing = get(&router, "/api/v1/connectors/nope", "tok-alice").await;
    let hidden = get(&router, "/api/v1/connectors/crm", "tok-bob").await;
    expect(&missing, 404, "connector.not_found");
    assert_eq!(strip(&missing), strip(&hidden), "no existence oracle");
    // erp_sync exists on acme only: on beta's host it is the same 404.
    let path = "/api/v1/connectors/erp_sync";
    let foreign = call(&router, "GET", HOST_B, path, Some("tok-alice"), &[], None).await;
    expect(&foreign, 404, "connector.not_found");
}

#[tokio::test]
async fn list_and_get_refuse_a_malformed_request_before_the_port() {
    let router = api_router(&fixture());
    for query in [
        "limit=0",
        "limit=101",
        "limit=1&limit=2",
        "limit=abc",
        "cursor=a&cursor=b",
    ] {
        let reply = get(&router, &format!("/api/v1/connectors?{query}"), "tok-alice").await;
        expect(&reply, 400, "api.invalid_request");
    }
    for query in ["cursor=has%20space", "filter=x", "cursor=zz"] {
        let reply = get(&router, &format!("/api/v1/connectors?{query}"), "tok-alice").await;
        expect(&reply, 400, "api.invalid_request");
    }
    let long = format!("/api/v1/connectors/{}", "n".repeat(201));
    expect(
        &get(&router, &long, "tok-alice").await,
        400,
        "api.invalid_request",
    );
}

// connector_invoke

const ECHO: &str = "/api/v1/connectors/crm/operations/echo";

async fn inv(router: &axum::Router, path: &str, token: &str, key: &str, body: Json) -> Reply {
    invoke(router, path, token, Some(key), body).await
}

#[tokio::test]
async fn invoke_runs_as_the_caller_and_answers_200_or_202() {
    let port = fixture();
    let router = api_router(&port);
    let reply = inv(
        &router,
        ECHO,
        "tok-alice",
        "k1",
        json!({"input": {"customer": "C-1"}}),
    )
    .await;
    expect(&reply, 200, "");
    assert_eq!(reply.json(), json!({"data": {"output": {"echoed": true}}}));
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    let calls = port.invocations();
    assert_eq!(
        (
            calls[0].0.as_str(),
            calls[0].1.as_str(),
            calls[0].2.as_str()
        ),
        (ACME, "crm", "echo")
    );
    assert_eq!(calls[0].3["customer"], "C-1");
    let long = "/api/v1/connectors/crm/operations/export";
    let job = inv(&router, long, "tok-alice", "k-long", json!({"input": {}})).await;
    expect(&job, 202, "");
    assert_eq!(job.json()["data"]["output"], json!({}));
    assert_eq!(
        job.json()["data"]["job_id"],
        "01950000-0000-7000-8000-000000000001"
    );
}

#[tokio::test]
async fn invoke_requires_an_idempotency_key_of_the_documented_form() {
    let port = fixture();
    let router = api_router(&port);
    let body = json!({"input": {"customer": "C"}});
    expect(
        &invoke(&router, ECHO, "tok-alice", None, body.clone()).await,
        400,
        "api.invalid_request",
    );
    for key in ["has space".to_owned(), "k".repeat(129)] {
        expect(
            &inv(&router, ECHO, "tok-alice", &key, body.clone()).await,
            400,
            "",
        );
    }
    assert!(
        port.invocations().is_empty(),
        "no refused call reached the connector"
    );
}

#[tokio::test]
async fn a_repeated_key_replays_the_first_answer_and_another_body_conflicts() {
    let port = fixture();
    let router = api_router(&port);
    let a = json!({"input": {"customer": "A"}});
    let first = inv(&router, ECHO, "tok-alice", "k-r", a.clone()).await;
    let again = inv(&router, ECHO, "tok-alice", "k-r", a).await;
    expect(&first, 200, "");
    expect(&again, 200, "");
    assert_eq!(again.header("idempotency-replayed"), Some("true"));
    assert_eq!(first.json(), again.json());
    assert_eq!(
        port.invocations().len(),
        1,
        "the replay did not call the connector again"
    );
    let other = inv(
        &router,
        ECHO,
        "tok-alice",
        "k-r",
        json!({"input": {"customer": "B"}}),
    )
    .await;
    expect(&other, 409, "api.idempotency_conflict");
}

#[tokio::test]
async fn invoke_refusals_use_the_registry_codes() {
    let router = api_router(&fixture());
    let fits = json!({"input": {"customer": "C"}});
    let empty = json!({"input": {}});
    #[rustfmt::skip]
    let cases: Vec<(&str, &str, &str, &Json, u16, &str)> = vec![
        ("op", "/api/v1/connectors/crm/operations/x", "tok-alice", &empty, 404, "connector.operation_unknown"),
        ("none", "/api/v1/connectors/nope/operations/echo", "tok-alice", &empty, 404, "connector.not_found"),
        ("hidden", ECHO, "tok-bob", &fits, 404, "connector.not_found"),
        ("disabled", "/api/v1/connectors/legacy/operations/echo", "tok-alice", &empty, 409, "connector.disabled"),
        ("no right", ECHO, "tok-carol", &fits, 403, "permission.denied"),
        ("bad input", ECHO, "tok-alice", &empty, 422, "connector.input_invalid"),
    ];
    for (label, path, token, body, status, code) in cases {
        let reply = inv(
            &router,
            path,
            token,
            &format!("k-{}", label.replace(' ', "-")),
            body.clone(),
        )
        .await;
        expect(&reply, status, code);
        if label == "bad input" {
            assert_eq!(
                reply.json()["error"]["details"]["fields"],
                json!(["/customer"])
            );
        }
    }
}

#[tokio::test]
async fn invoke_refuses_a_malformed_body_before_the_port() {
    let port = fixture();
    let router = api_router(&port);
    let many: serde_json::Map<String, Json> =
        (0..201).map(|i| (format!("f{i}"), Json::from(i))).collect();
    let bad = [
        json!([1]),
        json!({}),
        json!({"input": "x"}),
        json!({"input": [1]}),
    ];
    let bad = bad.into_iter().chain([
        json!({"input": {}, "as_user": "root"}),
        json!({"input": many}),
    ]);
    for (i, body) in bad.enumerate() {
        expect(
            &inv(&router, ECHO, "tok-alice", &format!("k{i}"), body).await,
            400,
            "",
        );
    }
    let extra = [("idempotency-key", "k-ct"), ("content-type", "text/plain")];
    let text = call(
        &router,
        "POST",
        HOST,
        ECHO,
        Some("tok-alice"),
        &extra,
        Some(json!({})),
    )
    .await;
    expect(&text, 415, "");
    assert!(port.invocations().is_empty());
}

// connector_runs and fail closed

#[tokio::test]
async fn the_run_log_has_no_inputs_or_outputs_pages_newest_first_and_needs_the_right() {
    let router = api_router(&fixture());
    let reply = get(&router, "/api/v1/connectors/crm/runs?limit=2", "tok-alice").await;
    expect(&reply, 200, "");
    let body = reply.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data[0]["at"], "2026-11-12T11:00:00Z");
    assert_eq!(data[1]["code"], "connector.upstream_failed");
    for line in data {
        let keys: Vec<&String> = line.as_object().unwrap().keys().collect();
        let allowed = ["at", "operation", "outcome", "duration_ms", "code"];
        assert!(
            keys.iter().all(|k| allowed.contains(&k.as_str())),
            "{keys:?}"
        );
    }
    let cursor = body["next_cursor"].as_str().unwrap();
    let path = format!("/api/v1/connectors/crm/runs?limit=2&cursor={cursor}");
    let page2 = get(&router, &path, "tok-alice").await.json();
    assert_eq!(page2["data"].as_array().unwrap().len(), 1);
    assert!(page2["next_cursor"].is_null());
    let runs = "/api/v1/connectors/crm/runs";
    expect(
        &get(&router, runs, "tok-carol").await,
        403,
        "permission.denied",
    );
    expect(
        &get(&router, runs, "tok-bob").await,
        404,
        "connector.not_found",
    );
}

#[tokio::test]
async fn with_no_port_every_operation_is_501_and_with_no_credential_401() {
    let router = bare_router();
    let key = [("idempotency-key", "k")];
    let reads = [
        "/api/v1/connectors",
        "/api/v1/connectors/crm",
        "/api/v1/connectors/crm/runs",
    ];
    for path in reads {
        expect(
            &call(&router, "GET", HOST, path, Some("t"), &[], None).await,
            501,
            "service.not_configured",
        );
    }
    let post = call(
        &router,
        "POST",
        HOST,
        ECHO,
        Some("t"),
        &key,
        Some(json!({"input": {}})),
    )
    .await;
    expect(&post, 501, "service.not_configured");
    let router = api_router(&fixture());
    for path in reads {
        expect(
            &call(&router, "GET", HOST, path, None, &[], None).await,
            401,
            "auth.required",
        );
    }
}

// The ingress

async fn send(
    router: &axum::Router,
    host: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Body,
) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, host);
    for (name, value) in headers {
        builder = builder.header(*name, value.as_str());
    }
    finish(
        router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap(),
    )
    .await
}

type Headers = Vec<(&'static str, String)>;

fn plus(mut headers: Headers, name: &'static str, value: &str) -> Headers {
    headers.push((name, value.to_owned()));
    headers
}

fn bare() -> Headers {
    plus(Vec::new(), "content-type", "application/json")
}

fn at(key: &[u8], t: i64) -> Headers {
    signed_with(key, t, DELIVERY_BODY)
}

fn signed(body: &[u8]) -> Headers {
    signed_with(SECRET, NOW, body)
}

fn signed_with(key: &[u8], t: i64, body: &[u8]) -> Headers {
    plus(
        bare(),
        "x-spot-signature",
        &inbound::signature_header(key, t, body),
    )
}

fn good() -> Headers {
    signed(DELIVERY_BODY)
}

async fn deliver(router: &axum::Router, name: &str, headers: Headers, body: &[u8]) -> Reply {
    let path = format!("/api/v1/connectors/{name}/inbound");
    send(router, HOST, &path, &headers, Body::from(body.to_vec())).await
}

async fn dlv(router: &axum::Router, name: &str, headers: Headers) -> Reply {
    deliver(router, name, headers, DELIVERY_BODY).await
}

#[tokio::test]
async fn a_correctly_signed_delivery_is_202_and_enqueued_as_parsed_json() {
    let port = fixture();
    let reply = dlv(&inbound_router(&port), "crm", good()).await;
    expect(&reply, 202, "");
    assert_eq!(reply.json(), json!({"data": {"accepted": true}}));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    let jobs = port.enqueued();
    assert_eq!(
        (
            jobs.len(),
            jobs[0].site.as_str(),
            jobs[0].connector.as_str()
        ),
        (1, ACME, "crm")
    );
    assert_eq!(jobs[0].body["order"], "SO-77");
}

#[tokio::test]
async fn the_edge_of_the_timestamp_window_is_inclusive_and_beyond_it_is_refused() {
    let router = inbound_router(&fixture());
    for t in [NOW - 300, NOW + 300, NOW] {
        expect(&dlv(&router, "crm", at(SECRET, t)).await, 202, "");
    }
    for t in [NOW - 301, NOW + 301, 0] {
        expect(
            &dlv(&router, "crm", at(SECRET, t)).await,
            401,
            "connector.signature_invalid",
        );
    }
}

#[tokio::test]
async fn every_unverified_request_is_the_same_401_and_the_port_never_enqueues() {
    let port = fixture();
    let router = inbound_router(&port);
    let bearer = format!("Bearer {}", String::from_utf8_lossy(BEARER));
    let sig = |v: &str| plus(bare(), "x-spot-signature", v);
    let id = |v: &'static str| plus(good(), "x-spot-delivery", v);
    #[rustfmt::skip]
    let cases: Vec<(&str, &str, Headers)> = vec![
        ("the wrong secret", "crm", at(b"not-the-secret", NOW)),
        ("a stale timestamp", "crm", at(SECRET, NOW - 400)),
        ("a future timestamp", "crm", at(SECRET, NOW + 400)),
        ("an unknown connector", "nobody", good()),
        ("no signature header", "crm", bare()),
        ("an empty signature", "crm", sig("")),
        ("a malformed signature", "crm", sig("t=1;v1=zz")),
        ("an over-long signature", "crm", sig(&"s".repeat(301))),
        ("a repeated signature", "crm", plus(good(), "x-spot-signature", &good()[1].1)),
        ("a bearer on an HMAC connector", "crm", plus(bare(), "authorization", &bearer)),
        ("a delivery id with a space", "crm", id("a b")),
        ("a delivery id given twice", "crm", plus(id("a"), "x-spot-delivery", "b")),
        ("a name with a control character", "bad%01name", good()),
    ];
    let mut flipped = DELIVERY_BODY.to_vec();
    flipped[10] ^= 1;
    let mut replies = vec![(
        "a flipped body byte",
        deliver(&router, "crm", good(), &flipped).await,
    )];
    for (label, name, headers) in cases {
        replies.push((label, dlv(&router, name, headers).await));
    }
    let scrub = |reply: &Reply| {
        let mut json = reply.json();
        json["error"].as_object_mut().unwrap().remove("request_id");
        json.to_string()
    };
    for (label, reply) in &replies {
        expect(reply, 401, "connector.signature_invalid");
        assert_eq!(
            scrub(reply),
            scrub(&replies[0].1),
            "{label} differs from the first refusal"
        );
        assert_eq!(strip(reply), strip(&replies[0].1), "{label}");
    }
    assert!(port.enqueued().is_empty());
    assert_eq!(
        port.receive_calls(),
        0,
        "an unverified request never reaches inbound_receive"
    );
}

#[tokio::test]
async fn a_refusal_names_neither_the_connector_nor_the_body_nor_the_secret() {
    let router = inbound_router(&fixture());
    let text = dlv(&router, "crm", at(b"wrong-key-material", NOW))
        .await
        .text();
    for leaked in [
        "crm",
        "SO-77",
        "120.50",
        "hunter2",
        "wrong-key",
        "v1=",
        "order.created",
    ] {
        assert!(!text.contains(leaked), "{leaked} is echoed: {text}");
    }
}

#[tokio::test]
async fn a_replayed_delivery_id_is_409_only_to_a_request_that_verified() {
    let port = fixture();
    let router = inbound_router(&port);
    let with_id = |h: Headers| plus(h, "x-spot-delivery", "delivery-1");
    expect(&dlv(&router, "crm", with_id(good())).await, 202, "");
    expect(
        &dlv(&router, "crm", with_id(good())).await,
        409,
        "connector.replayed",
    );
    assert_eq!(port.enqueued().len(), 1, "the replay started nothing");
    // The same id with a bad signature learns nothing about replays.
    let forged = dlv(&router, "crm", with_id(at(b"forged", NOW))).await;
    expect(&forged, 401, "connector.signature_invalid");
    // Another id is a new delivery; no id at all is never a replay.
    expect(
        &dlv(
            &router,
            "crm",
            plus(good(), "x-spot-delivery", "delivery-2"),
        )
        .await,
        202,
        "",
    );
    for _ in 0..2 {
        expect(&dlv(&router, "crm", good()).await, 202, "");
    }
}

#[tokio::test]
async fn a_verified_body_that_is_not_an_object_is_400_but_an_unverified_one_is_never_parsed() {
    let router = inbound_router(&fixture());
    for body in [&b"[1,2]"[..], b"{nope", b"", b"\"text\""] {
        expect(&deliver(&router, "crm", signed(body), body).await, 400, "");
    }
    let junk = deliver(&router, "crm", signed_with(b"x", NOW, b"{nope"), b"{nope").await;
    expect(&junk, 401, "connector.signature_invalid");
    expect(
        &dlv(&router, "legacy", good()).await,
        409,
        "connector.disabled",
    );
    let text = plus(vec![], "content-type", "text/plain");
    let wrong_type = dlv(&router, "crm", plus(text, "x-spot-signature", &good()[1].1)).await;
    expect(&wrong_type, 415, "");
}

/// A body that never ends: 16 KiB frames for ever, counting how many were polled.
struct Endless {
    polled: Arc<AtomicUsize>,
}

impl hyper::body::Body for Endless {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.polled.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Some(Ok(Frame::data(Bytes::from(vec![b'a'; 16 * 1024])))))
    }
}

#[tokio::test]
async fn the_body_is_cut_at_the_cap_before_the_check_and_before_the_port() {
    let port = fixture();
    let router = inbound_router(&port);
    let path = "/api/v1/connectors/crm/inbound";
    let polled = Arc::new(AtomicUsize::new(0));
    let endless = Body::new(Endless {
        polled: polled.clone(),
    });
    let reply = send(&router, HOST, path, &good(), endless).await;
    expect(&reply, 413, "http.body_too_large");
    let frames = polled.load(Ordering::SeqCst);
    assert!(
        frames <= OVERSIZE / (16 * 1024) + 4,
        "the read stopped near the cap: {frames}"
    );
    // A declared length over the cap is refused without reading a byte.
    let declared = plus(good(), "content-length", &(OVERSIZE + 1).to_string());
    let big = send(
        &router,
        HOST,
        path,
        &declared,
        Body::from(vec![b'x'; OVERSIZE + 1]),
    )
    .await;
    expect(&big, 413, "");
    assert_eq!(
        port.credential_lookups(),
        0,
        "nothing was checked, nothing was looked up"
    );
    assert!(port.enqueued().is_empty());
}

#[tokio::test]
async fn the_per_connector_and_per_source_limits_trigger_and_cover_unknown_names() {
    let port = Arc::new(MemoryConnectorsPort::with_limits(1000, 3));
    let mut crm = ConnectorSeed::new("crm");
    crm.inbound = Some(InboundCredential::Hmac(SECRET.to_vec()));
    port.add_connector(ACME, crm);
    let router = inbound_router(&port);
    for _ in 0..3 {
        expect(&dlv(&router, "crm", good()).await, 202, "");
    }
    let limited = dlv(&router, "crm", good()).await;
    expect(&limited, 429, "api.rate_limited");
    assert!(
        limited.json()["error"]["details"]["retry_after"]
            .as_u64()
            .unwrap()
            >= 1
    );
    for _ in 0..3 {
        expect(&dlv(&router, "ghost", good()).await, 401, "");
    }
    expect(
        &dlv(&router, "ghost", good()).await,
        429,
        "api.rate_limited",
    );
    // Two calls per source per minute, whatever the connector.
    let router = inbound_router(&Arc::new(MemoryConnectorsPort::with_limits(2, 1000)));
    for name in ["a", "b"] {
        expect(&dlv(&router, name, good()).await, 401, "");
    }
    expect(&dlv(&router, "c", good()).await, 429, "api.rate_limited");
}

#[tokio::test]
async fn the_bearer_scheme_works_when_the_authorization_header_reaches_the_route() {
    let router = inbound_router(&fixture());
    let token = String::from_utf8_lossy(BEARER).into_owned();
    let with = |value: String| plus(bare(), "authorization", &value);
    expect(
        &dlv(&router, "bearer_hook", with(format!("Bearer {token}"))).await,
        202,
        "",
    );
    for bad in [
        format!("Bearer {token}x"),
        "Bearer ".to_owned(),
        token,
        "Basic abc".to_owned(),
    ] {
        expect(&dlv(&router, "bearer_hook", with(bad)).await, 401, "");
    }
    // An HMAC signature on a bearer connector is refused.
    expect(&dlv(&router, "bearer_hook", good()).await, 401, "");
}

// Constant time, in the sense the route can show

#[tokio::test]
async fn unknown_and_known_but_bad_run_the_same_work_in_a_comparable_time() {
    let router = inbound_router(&fixture());
    let wrong = || at(b"wrong", NOW);
    let before = inbound::VERIFICATIONS.load(Ordering::SeqCst);
    expect(&dlv(&router, "nobody", wrong()).await, 401, "");
    let after_unknown = inbound::VERIFICATIONS.load(Ordering::SeqCst);
    expect(&dlv(&router, "crm", wrong()).await, 401, "");
    // Other tests in this binary may verify concurrently, so only a lower bound is exact.
    assert!(
        after_unknown > before,
        "the unknown name ran the verification"
    );
    assert!(
        inbound::VERIFICATIONS.load(Ordering::SeqCst) > after_unknown,
        "so did a bad one"
    );
    let (mut unknown, mut known_bad) = (Vec::new(), Vec::new());
    for _ in 0..80 {
        let start = Instant::now();
        dlv(&router, "nobody", wrong()).await;
        unknown.push(start.elapsed().as_nanos());
        let start = Instant::now();
        dlv(&router, "crm", wrong()).await;
        known_bad.push(start.elapsed().as_nanos());
    }
    let median = |mut v: Vec<u128>| {
        v.sort_unstable();
        v[v.len() / 2].max(1)
    };
    let (a, b) = (median(unknown), median(known_bad));
    // A bounded ratio, not an exact one: a data-dependent early return would be far outside it.
    assert!(
        a < b * 4 && b < a * 4,
        "unknown {a} ns vs known-but-bad {b} ns"
    );
}

// Through the registry

fn services_over(port: Option<Arc<dyn ConnectorsPort>>) -> ApiServices {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock.clone());
    let services = ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store);
    let services = services.with_clock(clock);
    match port {
        Some(port) => services.with_port::<dyn ConnectorsPort>(port),
        None => services,
    }
}

#[tokio::test]
async fn the_registered_group_is_public_in_the_auth_class_and_strips_the_authorization_header() {
    let port: Arc<dyn ConnectorsPort> = fixture();
    let (router, report) = router_with(services_over(Some(port)), &[inbound::registration()]);
    assert_eq!(report.mounted, vec!["connector_inbound"]);
    assert_eq!(report.public, vec!["connector_inbound"]);
    assert_eq!(
        inbound::registration().public.unwrap().rate_class,
        RateClass::Auth
    );
    let router = ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(router)
        .build();
    // The HMAC scheme works with no bearer at all.
    expect(&dlv(&router, "crm", good()).await, 202, "");
    // The registry's public layer removes Authorization: the bearer scheme cannot work here.
    let token = String::from_utf8_lossy(BEARER).into_owned();
    let headers = plus(bare(), "authorization", &format!("Bearer {token}"));
    expect(
        &dlv(&router, "bearer_hook", headers).await,
        401,
        "connector.signature_invalid",
    );
    // Without the port the group is not mounted.
    let (_, report) = router_with(services_over(None), &[inbound::registration()]);
    assert!(report.mounted.is_empty());
    assert_eq!(report.skipped.len(), 1);
}
