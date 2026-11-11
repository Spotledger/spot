//! Router plumbing shared by the suites: the full server router over the fake (`ServerBuilder
//! ::site_routes(api::router(services)).build()`), and request helpers that run through
//! `tower::ServiceExt::oneshot`. Site resolution, the server middlewares and the API layer's
//! middlewares all run, exactly as a mounted site sees them.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::http::header;
use serde_json::Value as Json;
use spot_app_api::prelude::*;
use tower::ServiceExt;

use spot_core::ManualClock;
use spot_http::ServerBuilder;
use spot_http::ServerConfig;
use spot_http::StaticSites;
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::router::{ApiServices, router};

use crate::support::Must;
use crate::support::fake::FakeApp;
use crate::support::permissions::{FakePermissions, FakeReveal};

pub const HOST: &str = "erp.acme.com";
pub const SITE: &str = "acme";

/// The router plus the handles a suite needs.
pub struct App {
    /// The full server router (site resolution included).
    pub router: axum::Router,
    /// The fake services bundle behind the router.
    pub fake: Arc<FakeApp>,
    /// The permission summary fake behind `auth_permissions`.
    pub permissions: Arc<FakePermissions>,
    /// The reveal fake behind `field_reveal`.
    pub reveal: Arc<FakeReveal>,
    /// The `Idempotency-Key` store the middleware uses.
    pub store: Arc<MemoryIdempotencyStore>,
    /// The manual clock the store (and the fake's invocation stamps) are timed by.
    pub clock: Arc<ManualClock>,
}

impl App {
    /// Default: a 64 KiB body limit, so big-body cases do not need a second server.
    pub fn new() -> App {
        App::with_max_body_bytes(64 * 1024)
    }

    /// A server with an explicit body limit (the 413 case).
    pub fn with_max_body_bytes(max_body_bytes: u64) -> App {
        let config = ServerConfig {
            max_body_bytes,
            ..ServerConfig::default()
        };
        App::build(config)
    }

    /// A server with an explicit configuration (trusted proxies, limits).
    pub fn build(config: ServerConfig) -> App {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
        let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
        let fake = FakeApp::new(clock.clone());
        let permissions = Arc::new(FakePermissions::new());
        let reveal = Arc::new(FakeReveal::new());
        let site = SiteId::new(SITE).must();
        let services = ApiServices::new(
            fake.clone(),
            fake.clone(),
            fake.clone(),
            fake.clone(),
            store.clone(),
        )
        .with_permissions(permissions.clone())
        .with_reveal(reveal.clone());
        let server_router = ServerBuilder::new(
            config,
            StaticSites::new()
                .add("erp.acme.com", site.clone())
                .add_site_name(site),
        )
        .site_routes(router(services))
        .build();
        App {
            router: server_router,
            fake,
            permissions,
            reveal,
            store,
            clock,
        }
    }
}

/// A full response: status, headers, parsed JSON, and the raw text for failure messages.
pub struct Call {
    /// The HTTP status.
    pub status: u16,
    /// The response headers.
    pub headers: axum::http::HeaderMap,
    /// The parsed JSON body (`Null` when the body is not JSON, e.g. an empty 204).
    pub body: Json,
    /// The raw body text.
    pub raw: String,
}

impl Call {
    /// A header value as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The error object of the envelope (empty object when there is none).
    pub fn error(&self) -> &Json {
        &self.body["error"]
    }

    /// The envelope's `error.code`.
    pub fn error_code(&self) -> &str {
        self.body["error"]["code"].as_str().must()
    }

    /// The `field` paths of the envelope's error `fields`, in order.
    pub fn field_paths(&self) -> Vec<String> {
        self.body["error"]["fields"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row["field"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The successful envelope's `data`.
    pub fn data(&self) -> &Json {
        &self.body["data"]
    }

    /// The document id of a successful document response.
    pub fn doc_id(&self) -> String {
        self.body["data"]["id"].as_str().must().to_owned()
    }

    /// The document version of a successful document response.
    pub fn doc_version(&self) -> u64 {
        self.body["data"]["version"].as_u64().must()
    }

    /// The `ETag` header of a document response.
    pub fn etag(&self) -> String {
        self.header("etag").must().to_owned()
    }

    /// The `x-request-id` header value.
    pub fn request_id(&self) -> &str {
        self.header("x-request-id").must()
    }

    /// Asserts the status; the message carries the whole body for the failure report.
    pub fn expect_status(&self, expected: u16, what: &str) -> &Self {
        assert_eq!(self.status, expected, "{what}: {}", self.raw);
        self
    }
}

/// The `Authorization: Bearer <token>` header pair.
pub fn bearer(token: &str) -> (&'static str, String) {
    ("authorization", format!("Bearer {token}"))
}

/// The `Idempotency-Key` header pair.
pub fn idem_key(key: &str) -> (&'static str, String) {
    ("idempotency-key", key.to_owned())
}

/// The `If-Match: "<version>"` header pair, from a numeric version.
pub fn if_match(version: u64) -> (&'static str, String) {
    ("if-match", format!("\"{version}\""))
}

/// One request through the whole router: body limit, site resolution, request id, auth and
/// idempotency middlewares all run. `Content-Type: application/json` is set when a body is
/// given; `headers` are appended verbatim.
pub async fn call(
    app: &App,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<Json>,
) -> Call {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    let mut body_bytes: Vec<u8> = Vec::new();
    let declares_content_type = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
    if let Some(body) = body {
        if !declares_content_type {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        body_bytes = serde_json::to_vec(&body).must();
    }
    for (name, value) in headers {
        builder = builder.header(*name, value.clone());
    }
    // The request head is dropped before the first await: `Request` and `Body` are not `Sync`.
    let request = match builder.body(Body::from(body_bytes)) {
        Ok(request) => request,
        Err(error) => panic!("invalid test request: {error}"),
    };
    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("router service failed: {error}"));
    let status = response.status().as_u16();
    let response_headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_else(|error| panic!("body not readable: {error}"));
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body_json = serde_json::from_str::<Json>(&raw).unwrap_or(Json::Null);
    Call {
        status,
        headers: response_headers,
        body: body_json,
        raw,
    }
}

/// `POST /api/v1/resource/{doctype}` — create.
pub async fn create_doc(app: &App, token: &str, doctype: &str, body: Json) -> Call {
    call(
        app,
        "POST",
        &format!("/api/v1/resource/{doctype}"),
        &[bearer(token)],
        Some(body),
    )
    .await
}

/// `GET /api/v1/resource/{doctype}/{id}` — load.
pub async fn get_doc(app: &App, token: &str, doctype: &str, id: &str) -> Call {
    call(
        app,
        "GET",
        &format!("/api/v1/resource/{doctype}/{id}"),
        &[bearer(token)],
        None,
    )
    .await
}

/// `PATCH /api/v1/resource/{doctype}/{id}` — merge update; headers carry `If-Match`.
pub async fn patch_doc(
    app: &App,
    token: &str,
    doctype: &str,
    id: &str,
    headers: &[(&str, String)],
    body: Json,
) -> Call {
    let mut all: Vec<(&str, String)> = vec![bearer(token)];
    all.extend(headers.iter().cloned());
    call(
        app,
        "PATCH",
        &format!("/api/v1/resource/{doctype}/{id}"),
        &all,
        Some(body),
    )
    .await
}

/// `POST /api/v1/resource/{doctype}/{id}/post` — lifecycle post; headers carry `If-Match`.
pub async fn post_doc(
    app: &App,
    token: &str,
    doctype: &str,
    id: &str,
    headers: &[(&str, String)],
) -> Call {
    let mut all: Vec<(&str, String)> = vec![bearer(token)];
    all.extend(headers.iter().cloned());
    call(
        app,
        "POST",
        &format!("/api/v1/resource/{doctype}/{id}/post"),
        &all,
        None,
    )
    .await
}

/// `POST /api/v1/resource/{doctype}/{id}/cancel` — lifecycle cancel; headers carry `If-Match`.
pub async fn cancel_doc(
    app: &App,
    token: &str,
    doctype: &str,
    id: &str,
    headers: &[(&str, String)],
) -> Call {
    let mut all: Vec<(&str, String)> = vec![bearer(token)];
    all.extend(headers.iter().cloned());
    call(
        app,
        "POST",
        &format!("/api/v1/resource/{doctype}/{id}/cancel",),
        &all,
        None,
    )
    .await
}

/// `DELETE /api/v1/resource/{doctype}/{id}`; headers carry `If-Match`.
pub async fn delete_doc(
    app: &App,
    token: &str,
    doctype: &str,
    id: &str,
    headers: &[(&str, String)],
) -> Call {
    let mut all: Vec<(&str, String)> = vec![bearer(token)];
    all.extend(headers.iter().cloned());
    call(
        app,
        "DELETE",
        &format!("/api/v1/resource/{doctype}/{id}"),
        &all,
        None,
    )
    .await
}

/// `GET /api/v1/meta/{doctype}`; headers may carry `If-None-Match`.
pub async fn get_meta(app: &App, token: &str, doctype: &str, headers: &[(&str, String)]) -> Call {
    let mut all: Vec<(&str, String)> = vec![bearer(token)];
    all.extend(headers.iter().cloned());
    call(app, "GET", &format!("/api/v1/meta/{doctype}"), &all, None).await
}

/// Creates a customer with a unique name; returns the `Call` (status 201 asserted by the caller
/// where it matters).
pub async fn seed_customer(app: &App, token: &str, name: &str) -> Call {
    create_doc(app, token, "customer", serde_json::json!({ "name": name })).await
}

/// One request under `host` instead of the default site host, for site-resolution probes.
pub async fn call_for_host(app: &App, host: &str, method: &str, path: &str) -> Call {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host)
        .body(Body::empty())
        .must();
    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("router service failed: {error}"));
    let status = response.status().as_u16();
    let response_headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_else(|error| panic!("body not readable: {error}"));
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    Call {
        status,
        headers: response_headers,
        body: serde_json::from_str::<Json>(&raw).unwrap_or(Json::Null),
        raw,
    }
}
