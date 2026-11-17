//! The public webhook receiver (`process_webhook`, `POST /api/v1/process/hook/{trigger}`,
//! WP-BPM-02.T5) over [`MemoryWebhookPort`]: the one route of the process group served without a
//! bearer. What the tests prove is the route's half of the bargain: what is checked before the
//! port (media type, size cap while streaming, the forms of the trigger, the delivery id and the
//! signature header), that the route never parses the body, that every refusal is one answer, and
//! that the port is handed a context with no credential and the resolved site only.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[allow(dead_code)]
mod support;

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use hyper::body::Frame;
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::ports::webhook::{MemoryWebhookPort, WebhookPort, WebhookRejected};
use spot_http::api::router::routes::{
    self, RateClass, Registration, RouteState, SkipReason, webhook,
};
use spot_http::api::router::{ApiServices, router};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use support::fake::FakeApp;
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const HOST_B: &str = "erp.beta.com";
const SIGNATURE: &str = "t=1,v1=goodsignature";
const PATH: &str = "/api/v1/process/hook/orders";

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
    let acme = SiteId::new("acme").unwrap();
    let beta = SiteId::new("beta").unwrap();
    StaticSites::new()
        .add(HOST, acme.clone())
        .add(HOST_B, beta.clone())
        .add_site_name(acme)
        .add_site_name(beta)
}

fn router_over(port: Arc<dyn WebhookPort>) -> axum::Router {
    let mut ports = Ports::new();
    ports.provide::<dyn WebhookPort>(port);
    let state = RouteState::new(ports.subset(webhook::required_ports()).unwrap());
    ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(webhook::routes().with_state(state))
        .build()
}

struct Fixture {
    router: axum::Router,
    port: Arc<MemoryWebhookPort>,
}

fn fixture() -> Fixture {
    let port = Arc::new(MemoryWebhookPort::new());
    port.register("acme", "orders", SIGNATURE);
    Fixture {
        router: router_over(port.clone()),
        port,
    }
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

async fn send(
    router: &axum::Router,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, host);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(body).unwrap();
    finish(router.clone().oneshot(request).await.unwrap()).await
}

/// A well-formed delivery: JSON, signed.
async fn deliver(f: &Fixture, extra: &[(&str, &str)], body: &[u8]) -> Reply {
    let mut headers = vec![
        ("content-type", "application/json"),
        ("x-spot-signature", SIGNATURE),
    ];
    headers.extend_from_slice(extra);
    send(&f.router, HOST, PATH, &headers, Body::from(body.to_vec())).await
}

const BODY: &[u8] = br#"{"order":"SO-1","amount":"10.00"}"#;

// ------------------------------------------------------------------------------------------
// Acceptance

#[tokio::test]
async fn a_signed_delivery_is_202_accepted_with_no_bearer() {
    let f = fixture();
    let reply = deliver(&f, &[], BODY).await;
    assert_eq!(reply.status, 202, "{}", reply.text());
    assert_eq!(reply.json(), json!({"accepted": true}));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert!(reply.header("x-request-id").is_some());
    let accepted = f.port.accepted();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].site, "acme");
    assert_eq!(accepted[0].trigger, "orders");
    assert_eq!(
        accepted[0].body,
        BODY.to_vec(),
        "the port gets the raw bytes"
    );
}

#[tokio::test]
async fn a_charset_parameter_on_the_media_type_is_accepted() {
    let f = fixture();
    let reply = send(
        &f.router,
        HOST,
        PATH,
        &[
            ("content-type", "Application/JSON; charset=utf-8"),
            ("x-spot-signature", SIGNATURE),
        ],
        Body::from(BODY.to_vec()),
    )
    .await;
    assert_eq!(reply.status, 202, "{}", reply.text());
}

#[tokio::test]
async fn the_response_never_says_whether_a_run_started_and_never_echoes_the_request() {
    let f = fixture();
    let reply = deliver(&f, &[], BODY).await;
    let text = reply.text();
    assert_eq!(text, r#"{"accepted":true}"#);
    for secret in ["SO-1", "10.00", "goodsignature", "orders"] {
        assert!(!text.contains(secret), "{secret} is echoed: {text}");
    }
    let rejected = send(
        &f.router,
        HOST,
        PATH,
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", "t=1,v1=badbadbad"),
        ],
        Body::from(BODY.to_vec()),
    )
    .await;
    let text = rejected.text();
    for secret in ["SO-1", "10.00", "badbadbad", "orders"] {
        assert!(!text.contains(secret), "{secret} is echoed: {text}");
    }
}

#[tokio::test]
async fn a_replayed_delivery_is_the_same_202_and_starts_nothing_twice() {
    let f = fixture();
    let first = deliver(&f, &[("idempotency-key", "delivery-1")], BODY).await;
    let again = deliver(&f, &[("idempotency-key", "delivery-1")], BODY).await;
    assert_eq!(first.status, 202);
    assert_eq!(again.status, 202);
    assert_eq!(strip(&first), strip(&again));
    assert_eq!(
        f.port.accepted().len(),
        1,
        "the replay is not delivered twice"
    );
    // With no key the delivery is identified by the body: a redelivery is the same delivery.
    let a = deliver(&f, &[], br#"{"x":1}"#).await;
    let b = deliver(&f, &[], br#"{"x":1}"#).await;
    assert_eq!((a.status, b.status), (202, 202));
    assert_eq!(f.port.accepted().len(), 2);
    let c = deliver(&f, &[], br#"{"x":2}"#).await;
    assert_eq!(c.status, 202);
    assert_eq!(f.port.accepted().len(), 3);
}

// ------------------------------------------------------------------------------------------
// One rejection

#[tokio::test]
async fn every_rejection_is_the_same_401() {
    let f = fixture();
    let json_ct = ("content-type", "application/json");
    let good_sig = ("x-spot-signature", SIGNATURE);
    let long_signature = "s".repeat(513);
    let long_trigger_path = format!("/api/v1/process/hook/{}", "t".repeat(65));
    type Case<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>, Vec<u8>);
    let cases: Vec<Case> = vec![
        (
            "a bad signature",
            PATH,
            vec![json_ct, ("x-spot-signature", "t=1,v1=nope")],
            BODY.to_vec(),
        ),
        ("a missing signature", PATH, vec![json_ct], BODY.to_vec()),
        (
            "an empty signature",
            PATH,
            vec![json_ct, ("x-spot-signature", "")],
            BODY.to_vec(),
        ),
        (
            "a repeated signature",
            PATH,
            vec![json_ct, good_sig, good_sig],
            BODY.to_vec(),
        ),
        (
            "an over-long signature",
            PATH,
            vec![json_ct, ("x-spot-signature", long_signature.as_str())],
            BODY.to_vec(),
        ),
        (
            "an unknown trigger",
            "/api/v1/process/hook/nobody",
            vec![json_ct, good_sig],
            BODY.to_vec(),
        ),
        (
            "a trigger in the wrong form",
            "/api/v1/process/hook/Bad_Trigger",
            vec![json_ct, good_sig],
            BODY.to_vec(),
        ),
        (
            "a long trigger",
            long_trigger_path.as_str(),
            vec![json_ct, good_sig],
            BODY.to_vec(),
        ),
        (
            "a body that is not a JSON object",
            PATH,
            vec![json_ct, good_sig],
            b"[1,2,3]".to_vec(),
        ),
        (
            "a body that is not JSON at all",
            PATH,
            vec![json_ct, good_sig],
            b"{nope".to_vec(),
        ),
        ("an empty body", PATH, vec![json_ct, good_sig], Vec::new()),
        (
            "a delivery id with a space",
            PATH,
            vec![json_ct, good_sig, ("idempotency-key", "has space")],
            BODY.to_vec(),
        ),
        (
            "a delivery id given twice",
            PATH,
            vec![
                json_ct,
                good_sig,
                ("idempotency-key", "a"),
                ("idempotency-key", "b"),
            ],
            BODY.to_vec(),
        ),
    ];
    let mut answers = Vec::new();
    for (what, path, headers, body) in cases {
        let reply = send(&f.router, HOST, path, &headers, Body::from(body)).await;
        assert_eq!(reply.status, 401, "{what}: {}", reply.text());
        assert_eq!(reply.code(), "process.webhook_rejected", "{what}");
        assert_eq!(reply.header("cache-control"), Some("no-store"), "{what}");
        assert!(reply.json()["error"].get("details").is_none(), "{what}");
        assert!(reply.json()["error"].get("fields").is_none(), "{what}");
        answers.push((what, strip(&reply)));
    }
    let (first_what, first) = &answers[0];
    for (what, answer) in &answers {
        assert_eq!(answer, first, "{what} differs from {first_what}");
    }
    assert!(f.port.accepted().is_empty());
}

#[tokio::test]
async fn a_body_is_not_parsed_by_the_route_so_bad_json_with_a_bad_signature_is_a_401_not_a_400() {
    let f = fixture();
    let reply = send(
        &f.router,
        HOST,
        PATH,
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", "t=1,v1=nope"),
        ],
        Body::from("{ this is not json"),
    )
    .await;
    assert_eq!(reply.status, 401, "{}", reply.text());
    assert_eq!(reply.code(), "process.webhook_rejected");
    assert_ne!(reply.code(), "api.invalid_json");
}

#[tokio::test]
async fn an_unknown_trigger_and_a_bad_signature_cannot_be_told_apart() {
    let f = fixture();
    let unknown = send(
        &f.router,
        HOST,
        "/api/v1/process/hook/nobody",
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", SIGNATURE),
        ],
        Body::from(BODY.to_vec()),
    )
    .await;
    let bad = send(
        &f.router,
        HOST,
        PATH,
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", "t=1,v1=nope"),
        ],
        Body::from(BODY.to_vec()),
    )
    .await;
    assert_eq!(strip(&unknown), strip(&bad));
}

// ------------------------------------------------------------------------------------------
// Before the port

#[tokio::test]
async fn a_wrong_or_missing_media_type_is_415_before_the_port() {
    let f = fixture();
    for (what, headers) in [
        (
            "text/plain",
            vec![
                ("content-type", "text/plain"),
                ("x-spot-signature", SIGNATURE),
            ],
        ),
        (
            "form",
            vec![
                ("content-type", "application/x-www-form-urlencoded"),
                ("x-spot-signature", SIGNATURE),
            ],
        ),
        ("none", vec![("x-spot-signature", SIGNATURE)]),
        (
            "twice",
            vec![
                ("content-type", "application/json"),
                ("content-type", "application/json"),
                ("x-spot-signature", SIGNATURE),
            ],
        ),
    ] {
        let reply = send(&f.router, HOST, PATH, &headers, Body::from(BODY.to_vec())).await;
        assert_eq!(reply.status, 415, "{what}: {}", reply.text());
        assert_eq!(reply.code(), "api.unsupported_media_type", "{what}");
    }
    assert_eq!(f.port.calls(), 0, "the port was never asked");
}

#[tokio::test]
async fn a_body_over_256_kib_is_413_before_the_port() {
    let f = fixture();
    let headers = [
        ("content-type", "application/json"),
        ("x-spot-signature", SIGNATURE),
    ];
    // The body is larger than the cap.
    let over = vec![b' '; webhook::MAX_BODY_BYTES + 1];
    let reply = send(&f.router, HOST, PATH, &headers, Body::from(over)).await;
    assert_eq!(reply.status, 413, "{}", reply.text());
    assert_eq!(reply.code(), "http.body_too_large");
    // A declared length over the cap is refused without reading the body.
    let declared = (webhook::MAX_BODY_BYTES + 1).to_string();
    let reply = send(
        &f.router,
        HOST,
        PATH,
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", SIGNATURE),
            ("content-length", declared.as_str()),
        ],
        Body::from("{}"),
    )
    .await;
    assert_eq!(reply.status, 413, "{}", reply.text());
    assert_eq!(f.port.calls(), 0, "the port was never asked");
    // A body exactly at the cap is read (and rejected by the port as it is not an object).
    let mut at_cap = b"{\"a\":\"".to_vec();
    at_cap.resize(webhook::MAX_BODY_BYTES - 2, b'x');
    at_cap.extend_from_slice(b"\"}");
    assert_eq!(at_cap.len(), webhook::MAX_BODY_BYTES);
    let reply = send(&f.router, HOST, PATH, &headers, Body::from(at_cap)).await;
    assert_eq!(reply.status, 202, "{}", reply.text());
    assert_eq!(f.port.calls(), 1);
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
async fn an_endless_body_is_cut_off_at_the_cap_while_it_streams() {
    let f = fixture();
    let polled = Arc::new(AtomicUsize::new(0));
    let body = Body::new(Endless {
        polled: polled.clone(),
    });
    let reply = send(
        &f.router,
        HOST,
        PATH,
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", SIGNATURE),
        ],
        body,
    )
    .await;
    assert_eq!(reply.status, 413, "{}", reply.text());
    assert_eq!(reply.code(), "http.body_too_large");
    assert_eq!(f.port.calls(), 0, "the port was never asked");
    let frames = polled.load(Ordering::SeqCst);
    let cap_frames = webhook::MAX_BODY_BYTES / (16 * 1024);
    assert!(
        frames <= cap_frames + 4,
        "the read stopped near the cap: {frames} frames of 16 KiB"
    );
}

// ------------------------------------------------------------------------------------------
// The context the port gets

/// What the probe saw of a call.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    site: String,
    credential_absent: bool,
    key_in_ctx: Option<String>,
    trigger: String,
    delivery: Option<String>,
    signature: String,
    body: Vec<u8>,
}

#[derive(Default)]
struct Probe {
    seen: Mutex<Vec<Seen>>,
}

impl WebhookPort for Probe {
    fn receive<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        trigger: &'a str,
        delivery_id: Option<&'a str>,
        signature: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<(), WebhookRejected>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(Seen {
                site: ctx.site.as_str().to_owned(),
                credential_absent: ctx.credential.is_none(),
                key_in_ctx: ctx.idempotency_key.clone(),
                trigger: trigger.to_owned(),
                delivery: delivery_id.map(str::to_owned),
                signature: signature.to_owned(),
                body: body.to_vec(),
            });
            Ok(())
        })
    }
}

#[tokio::test]
async fn the_port_gets_the_resolved_site_no_credential_and_the_raw_inputs() {
    let probe = Arc::new(Probe::default());
    let router = router_over(probe.clone());
    let reply = send(
        &router,
        HOST,
        PATH,
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", SIGNATURE),
            ("idempotency-key", "delivery-9"),
            // A bearer, a user header and a tenant header change nothing the port sees.
            ("authorization", "Bearer some-user-token"),
            ("x-actor", "alice"),
            ("x-site", "beta"),
        ],
        Body::from(BODY.to_vec()),
    )
    .await;
    assert_eq!(reply.status, 202, "{}", reply.text());
    let seen = probe.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![Seen {
            site: "acme".to_owned(),
            credential_absent: true,
            key_in_ctx: Some("delivery-9".to_owned()),
            trigger: "orders".to_owned(),
            delivery: Some("delivery-9".to_owned()),
            signature: SIGNATURE.to_owned(),
            body: BODY.to_vec(),
        }]
    );
}

#[tokio::test]
async fn a_missing_signature_still_reaches_the_port_as_an_empty_one() {
    let probe = Arc::new(Probe::default());
    let router = router_over(probe.clone());
    let reply = send(
        &router,
        HOST,
        PATH,
        &[("content-type", "application/json")],
        Body::from(BODY.to_vec()),
    )
    .await;
    // The probe accepts everything: the route took no branch of its own on the signature.
    assert_eq!(reply.status, 202, "{}", reply.text());
    assert_eq!(probe.seen.lock().unwrap()[0].signature, "");
}

#[tokio::test]
async fn a_trigger_of_one_site_is_nothing_on_another() {
    let f = fixture();
    let on_acme = deliver(&f, &[], BODY).await;
    assert_eq!(on_acme.status, 202);
    let on_beta = send(
        &f.router,
        HOST_B,
        PATH,
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", SIGNATURE),
        ],
        Body::from(BODY.to_vec()),
    )
    .await;
    assert_eq!(on_beta.status, 401, "{}", on_beta.text());
    assert_eq!(on_beta.code(), "process.webhook_rejected");
    assert_eq!(f.port.accepted().len(), 1);
    assert_eq!(f.port.accepted()[0].site, "acme");
}

// ------------------------------------------------------------------------------------------
// Fail closed

#[tokio::test]
async fn with_no_port_the_route_answers_501_not_a_pass() {
    let router = ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(webhook::routes().with_state(RouteState::new(Ports::new())))
        .build();
    let reply = send(
        &router,
        HOST,
        PATH,
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", SIGNATURE),
        ],
        Body::from(BODY.to_vec()),
    )
    .await;
    assert_eq!(reply.status, 501, "{}", reply.text());
    assert_eq!(reply.code(), "service.not_configured");
}

#[tokio::test]
async fn only_post_is_served() {
    let f = fixture();
    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let request = Request::builder()
            .method(method)
            .uri(PATH)
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap();
        let response = f.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 405, "{method}");
    }
    assert_eq!(f.port.calls(), 0);
}

// ------------------------------------------------------------------------------------------
// The group

#[test]
fn the_group_is_public_with_the_auth_rate_class_and_serves_one_operation() {
    assert_eq!(webhook::SERVES, &["process_webhook"]);
    assert_eq!(webhook::required_ports().len(), 1);
    assert_eq!(webhook::PUBLIC.rate_class, RateClass::Auth);
}

#[test]
fn the_served_id_is_an_operation_of_the_contract_without_security() {
    let contract = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    ))
    .unwrap();
    let lines: Vec<&str> = contract.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim() == "operationId: process_webhook")
        .expect("process_webhook is an operationId of the contract");
    // The operation's own block runs to the next path or operation.
    let block: Vec<&&str> = lines
        .iter()
        .skip(at)
        .take_while(|line| !line.starts_with("  /api/") && !line.starts_with("    post:"))
        .collect();
    assert!(
        block.iter().any(|line| line.trim() == "security: []"),
        "the operation is declared without a bearer"
    );
}

fn webhook_registration() -> Registration {
    Registration {
        name: "webhook",
        serves: webhook::SERVES,
        required_ports: webhook::required_ports,
        routes: webhook::routes,
        public: Some(webhook::PUBLIC),
    }
}

#[test]
fn the_registry_mounts_the_group_as_public_only_with_its_port() {
    let mounted = routes::mount(&[webhook_registration()], &Ports::new(), &[]);
    assert!(mounted.report.mounted.is_empty());
    assert!(matches!(
        mounted.report.skipped.as_slice(),
        [("webhook", SkipReason::MissingPort(_))]
    ));
    let mut ports = Ports::new();
    ports.provide::<dyn WebhookPort>(Arc::new(MemoryWebhookPort::new()));
    let mounted = routes::mount(&[webhook_registration()], &ports, &[]);
    assert_eq!(mounted.report.mounted, vec!["webhook"]);
    assert_eq!(mounted.report.public, vec!["webhook"]);
    assert_eq!(mounted.public.len(), 1);
    assert_eq!(mounted.public[0].0, RateClass::Auth);
}

// ------------------------------------------------------------------------------------------
// Through the real router: no bearer, no Authorization, and the rate limit

fn real_router(port: Arc<dyn WebhookPort>) -> axum::Router {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock.clone());
    let services = ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store)
        .with_clock(clock)
        .with_port::<dyn WebhookPort>(port);
    ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(router(services))
        .build()
}

async fn send_from(router: &axum::Router, from: &str, headers: &[(&str, &str)]) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(PATH)
        .header(header::HOST, HOST);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::from(BODY.to_vec())).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(from.parse().unwrap(), 40_000)));
    finish(router.clone().oneshot(request).await.unwrap()).await
}

#[tokio::test]
async fn through_the_router_no_bearer_is_demanded_and_none_reaches_the_port() {
    let probe = Arc::new(Probe::default());
    let router = real_router(probe.clone());
    let signed = [
        ("content-type", "application/json"),
        ("x-spot-signature", SIGNATURE),
    ];
    let anonymous = send_from(&router, "203.0.113.1", &signed).await;
    assert_eq!(anonymous.status, 202, "{}", anonymous.text());
    let with_bearer = send_from(
        &router,
        "203.0.113.2",
        &[
            ("content-type", "application/json"),
            ("x-spot-signature", SIGNATURE),
            ("authorization", "Bearer a-real-looking-token"),
        ],
    )
    .await;
    assert_eq!(with_bearer.status, 202, "{}", with_bearer.text());
    let seen = probe.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|s| s.credential_absent));
}

#[tokio::test]
async fn through_the_router_the_auth_rate_class_limits_each_client_address() {
    let router = real_router(Arc::new(MemoryWebhookPort::new()));
    let headers = [
        ("content-type", "application/json"),
        ("x-spot-signature", "t=1,v1=nope"),
    ];
    // Rejected deliveries count too: the limit is on the address, not on the outcome.
    let mut limited = None;
    for n in 0..40 {
        let reply = send_from(&router, "203.0.113.9", &headers).await;
        if reply.status == 429 {
            limited = Some((n, reply));
            break;
        }
        assert_eq!(reply.status, 401, "{}", reply.text());
    }
    let (n, reply) = limited.expect("the address was rate limited");
    assert_eq!(n, 20, "the auth class allows 20 requests per window");
    assert_eq!(reply.code(), "api.rate_limited");
    assert!(reply.header("retry-after").is_some());
    // Another address is not affected.
    let other = send_from(&router, "198.51.100.7", &headers).await;
    assert_eq!(other.status, 401, "{}", other.text());
}
