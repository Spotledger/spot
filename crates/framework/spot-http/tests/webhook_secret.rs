//! The `/api/v1/webhooks/{name}/signing-secret` routes (WP-INT-01.T5) over a fake
//! [`WebhookSecretPort`]: the shape of every request the routes accept or refuse, what is checked
//! before the port, what the responses show and what they never show.
//!
//! The fake decides nothing the real adapter decides; it answers what a test scripts and keeps what
//! the routes passed it (never a secret value, only its length and a redacted `Debug`), so the
//! tests prove the routes' half of the bargain.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::ports::webhook_secret::{
    MAX_SECRET_BYTES, MIN_SECRET_BYTES, SigningSecretIn, SigningSecretInfo, SigningSecretOut,
    SigningSecretText, WebhookSecretError, WebhookSecretPort,
};
use spot_http::api::router::routes::{self, RouteState, webhook_secret};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const TOKEN: &str = "tok-alice";
const SECRET: &str = "whsec_canary_5f1d9c7e3a2b4c68";
const GENERATED: &str = "generated_canary_Q7pL2mXv9RtK4wZ0aBcDeFgHiJkLmNoPqRs";

#[derive(Default)]
struct Script {
    /// The next call fails with this.
    fail: Option<WebhookSecretError>,
    /// What `get` answers.
    info: Option<SigningSecretInfo>,
    /// Calls the port received: `(operation, webhook, detail)`.
    calls: Vec<(&'static str, String, String)>,
    /// The `Debug` text of every input the port received.
    debug: Vec<String>,
}

struct Fake {
    script: Mutex<Script>,
}

impl Fake {
    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl WebhookSecretPort for Fake {
    fn put<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        webhook: &'a str,
        input: SigningSecretIn,
    ) -> BoxFuture<'a, Result<SigningSecretOut, WebhookSecretError>> {
        Box::pin(async move {
            let mut script = self.lock();
            script.debug.push(format!("{input:?}"));
            let (kind, generated) = match &input {
                SigningSecretIn::Supplied(text) => (format!("supplied:{}", text.len()), None),
                SigningSecretIn::Generate => (
                    "generate".to_owned(),
                    Some(SigningSecretText::new(GENERATED)),
                ),
            };
            script.calls.push(("put", webhook.to_owned(), kind));
            if let Some(error) = script.fail.take() {
                return Err(error);
            }
            Ok(SigningSecretOut {
                key_version: 2,
                fingerprint: "0123456789ab".to_owned(),
                generated,
            })
        })
    }

    fn get<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        webhook: &'a str,
    ) -> BoxFuture<'a, Result<SigningSecretInfo, WebhookSecretError>> {
        Box::pin(async move {
            let mut script = self.lock();
            script
                .calls
                .push(("get", webhook.to_owned(), String::new()));
            if let Some(error) = script.fail.take() {
                return Err(error);
            }
            Ok(script.info.clone().unwrap_or(SigningSecretInfo {
                set: true,
                key_version: 2,
                fingerprint: "0123456789ab".to_owned(),
            }))
        })
    }
}

struct Fixture {
    router: axum::Router,
    fake: Arc<Fake>,
}

fn fixture() -> Fixture {
    let fake = Arc::new(Fake {
        script: Mutex::new(Script::default()),
    });
    let mut ports = Ports::new();
    ports.provide::<dyn WebhookSecretPort>(fake.clone());
    let state = RouteState::new(ports.subset(webhook_secret::required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(webhook_secret::routes().with_state(state))
    .build();
    Fixture { router, fake }
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
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
    fn reason(&self) -> String {
        self.json()["error"]["details"]["reason"]
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

async fn send(
    f: &Fixture,
    method: &str,
    path: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    body: Option<String>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let request = builder
        .body(match body {
            Some(text) => Body::from(text),
            None => Body::empty(),
        })
        .unwrap();
    let response = f.router.clone().oneshot(request).await.unwrap();
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

const PATH: &str = "/api/v1/webhooks/orders/signing-secret";

async fn put(f: &Fixture, body: Json) -> Reply {
    send(
        f,
        "PUT",
        PATH,
        Some(TOKEN),
        Some("application/json"),
        Some(body.to_string()),
    )
    .await
}

async fn get(f: &Fixture) -> Reply {
    send(f, "GET", PATH, Some(TOKEN), None, None).await
}

fn calls(f: &Fixture) -> usize {
    f.fake.lock().calls.len()
}

fn fail_next(f: &Fixture, error: WebhookSecretError) {
    f.fake.lock().fail = Some(error);
}

// ------------------------------------------------------------------------------------------
// PUT: a supplied secret

#[tokio::test]
async fn a_supplied_secret_is_stored_and_never_echoed() {
    let f = fixture();
    let r = put(&f, json!({ "secret": SECRET })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert_eq!(
        r.json(),
        json!({"data": {"key_version": 2, "fingerprint": "0123456789ab"}})
    );
    assert!(!r.text().contains("canary"), "{}", r.text());
    assert!(
        !r.headers
            .iter()
            .any(|(_, v)| { v.to_str().map(|v| v.contains("canary")).unwrap_or(false) }),
        "no header holds the value"
    );
    let script = f.fake.lock();
    assert_eq!(
        script.calls,
        vec![(
            "put",
            "orders".to_owned(),
            format!("supplied:{}", SECRET.len())
        )]
    );
    // The type the port received formats with no value in it.
    assert!(
        script.debug.iter().all(|d| !d.contains("canary")),
        "{:?}",
        script.debug
    );
}

#[tokio::test]
async fn the_length_bounds_are_16_and_256_bytes() {
    let f = fixture();
    for len in [MIN_SECRET_BYTES, MAX_SECRET_BYTES] {
        let r = put(&f, json!({ "secret": "k".repeat(len) })).await;
        assert_eq!(r.status, 200, "{len}: {}", r.text());
    }
    assert_eq!(calls(&f), 2);
    let short = put(&f, json!({ "secret": "k".repeat(MIN_SECRET_BYTES - 1) })).await;
    assert_eq!(short.status, 400, "{}", short.text());
    assert_eq!(short.code(), "api.invalid_request");
    assert_eq!(short.reason(), "webhook_secret.too_short");
    let long = put(&f, json!({ "secret": "k".repeat(MAX_SECRET_BYTES + 1) })).await;
    assert_eq!(long.status, 400, "{}", long.text());
    assert_eq!(long.reason(), "webhook_secret.too_long");
    assert!(!short.text().contains("kkkk") && !long.text().contains("kkkk"));
    assert_eq!(calls(&f), 2, "a refused length never reaches the port");
}

// ------------------------------------------------------------------------------------------
// PUT: a generated secret

#[tokio::test]
async fn a_generated_secret_is_shown_once_in_the_put_response_only() {
    let f = fixture();
    let r = put(&f, json!({ "generate": true })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert_eq!(
        r.json(),
        json!({"data": {"key_version": 2, "fingerprint": "0123456789ab", "secret": GENERATED}})
    );
    // A read of the same webhook never shows it.
    let again = get(&f).await;
    assert_eq!(again.status, 200, "{}", again.text());
    assert!(!again.text().contains("canary"), "{}", again.text());
    assert!(again.json()["data"].get("secret").is_none());
    assert_eq!(f.fake.lock().calls[0].2, "generate");
}

// ------------------------------------------------------------------------------------------
// PUT: the shape of a request

#[tokio::test]
async fn a_malformed_body_is_refused_before_the_port_without_echoing_it() {
    let f = fixture();
    for body in [
        json!({}),
        json!({ "secret": SECRET, "generate": true }),
        json!({ "generate": false }),
        json!({ "generate": "yes" }),
        json!({ "secret": 12345 }),
        json!({ "secret": SECRET, "extra": 1 }),
        json!({ "Secret": SECRET }),
        json!([SECRET]),
        json!(SECRET),
        json!(null),
    ] {
        let r = put(&f, body.clone()).await;
        assert_eq!(r.status, 400, "{body}: {}", r.text());
        assert_eq!(r.code(), "api.invalid_request", "{body}");
        assert!(!r.text().contains("canary"), "{body}: {}", r.text());
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
    }
    assert_eq!(calls(&f), 0);
}

#[tokio::test]
async fn the_body_must_be_json_and_must_be_there() {
    let f = fixture();
    let plain = send(
        &f,
        "PUT",
        PATH,
        Some(TOKEN),
        Some("text/plain"),
        Some(SECRET.to_owned()),
    )
    .await;
    assert_eq!(plain.status, 415, "{}", plain.text());
    assert!(!plain.text().contains("canary"));
    let none = send(&f, "PUT", PATH, Some(TOKEN), Some("application/json"), None).await;
    assert_eq!(none.status, 400, "{}", none.text());
    let broken = send(
        &f,
        "PUT",
        PATH,
        Some(TOKEN),
        Some("application/json"),
        Some(format!("{{\"secret\": \"{SECRET}")),
    )
    .await;
    assert_eq!(broken.status, 400, "{}", broken.text());
    assert!(!broken.text().contains("canary"), "{}", broken.text());
    assert_eq!(calls(&f), 0);
}

#[tokio::test]
async fn a_secret_in_the_query_string_is_ignored_and_never_reaches_the_port() {
    let f = fixture();
    let r = send(
        &f,
        "PUT",
        &format!("{PATH}?secret={SECRET}"),
        Some(TOKEN),
        Some("application/json"),
        Some(json!({ "generate": true }).to_string()),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let script = f.fake.lock();
    assert_eq!(script.calls[0].2, "generate");
    assert!(script.debug.iter().all(|d| !d.contains("canary")));
}

#[tokio::test]
async fn a_webhook_name_that_cannot_be_a_name_is_refused_before_the_port() {
    let f = fixture();
    let long = "w".repeat(201);
    for name in ["%0A", "bad%00name", long.as_str()] {
        let path = format!("/api/v1/webhooks/{name}/signing-secret");
        let r = send(&f, "GET", &path, Some(TOKEN), None, None).await;
        assert_eq!(r.status, 400, "{name}: {}", r.text());
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
    }
    assert_eq!(calls(&f), 0);
    // A name with a space is a name: it is free text in the `name` field.
    let r = send(
        &f,
        "GET",
        "/api/v1/webhooks/Order%20events/signing-secret",
        Some(TOKEN),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(f.fake.lock().calls[0].1, "Order events");
}

// ------------------------------------------------------------------------------------------
// GET

#[tokio::test]
async fn a_get_answers_the_fingerprint_and_key_version_never_the_value() {
    let f = fixture();
    let r = get(&f).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert_eq!(
        r.json(),
        json!({"data": {"set": true, "key_version": 2, "fingerprint": "0123456789ab"}})
    );
    f.fake.lock().info = Some(SigningSecretInfo {
        set: false,
        key_version: 0,
        fingerprint: String::new(),
    });
    let unset = get(&f).await;
    assert_eq!(
        unset.json(),
        json!({"data": {"set": false, "key_version": 0, "fingerprint": ""}})
    );
}

// ------------------------------------------------------------------------------------------
// Authentication and the port's refusals

#[tokio::test]
async fn a_request_without_a_credential_is_refused_before_the_port() {
    let f = fixture();
    let r = send(&f, "GET", PATH, None, None, None).await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(r.code(), "auth.required");
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    let r = send(
        &f,
        "PUT",
        PATH,
        None,
        Some("application/json"),
        Some(json!({ "secret": SECRET }).to_string()),
    )
    .await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert!(!r.text().contains("canary"));
    assert_eq!(calls(&f), 0);
}

#[tokio::test]
async fn the_ports_refusals_are_answered_by_kind_and_carry_no_text() {
    for (error, status, code) in [
        (WebhookSecretError::Refused, 403, "permission.denied"),
        (WebhookSecretError::NotFound, 404, "document.not_found"),
        (
            WebhookSecretError::NotConfigured,
            501,
            "service.not_configured",
        ),
        (WebhookSecretError::Unavailable, 503, "server.unavailable"),
        (WebhookSecretError::Failed, 500, "http.internal"),
    ] {
        let f = fixture();
        fail_next(&f, error);
        let r = put(&f, json!({ "secret": SECRET })).await;
        assert_eq!(r.status, status, "{code}: {}", r.text());
        assert_eq!(r.code(), code);
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
        assert!(!r.text().contains("canary"), "{}", r.text());
        assert!(r.json()["error"]["request_id"].is_string());
    }
}

#[tokio::test]
async fn a_refusal_looks_the_same_for_a_webhook_that_exists_and_one_that_does_not() {
    let f = fixture();
    fail_next(&f, WebhookSecretError::Refused);
    let known = send(&f, "GET", PATH, Some(TOKEN), None, None).await;
    fail_next(&f, WebhookSecretError::Refused);
    let unknown = send(
        &f,
        "GET",
        "/api/v1/webhooks/no_such_hook/signing-secret",
        Some(TOKEN),
        None,
        None,
    )
    .await;
    assert_eq!(known.status, unknown.status);
    assert_eq!(known.code(), unknown.code());
    assert_eq!(known.reason(), unknown.reason());
    assert_eq!(
        known.json()["error"]["message"],
        unknown.json()["error"]["message"]
    );
}

// ------------------------------------------------------------------------------------------
// The group

#[tokio::test]
async fn the_group_is_not_mounted_without_its_port() {
    let mounted = routes::mount(&[webhook_secret::registration()], &Ports::new(), &[]);
    assert!(mounted.report.mounted.is_empty());
    assert_eq!(mounted.report.skipped.len(), 1);
}

#[test]
fn the_group_serves_exactly_the_two_signing_secret_operations() {
    assert_eq!(
        webhook_secret::SERVES,
        &["webhook_signing_secret_put", "webhook_signing_secret_get"]
    );
    let registered = routes::registry_serves();
    for id in webhook_secret::SERVES {
        assert!(registered.contains(id), "{id} is not registered");
    }
}
