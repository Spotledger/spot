//! The `/api/v1/auth/sessions` routes (`auth_session_list` and `auth_session_end`,
//! WP-IAM-07.T2) over a fake [`SessionPort`]: every status and error code the contract names for
//! the two operations, the shape of every request the routes accept or refuse, and the JSON the
//! contract specifies for a session record.
//!
//! The fake decides nothing the real adapter decides; it answers what a test scripts and keeps
//! what the routes passed it, so the tests prove the routes' half of the bargain: what is checked
//! before the port, what is passed to it (never a user id) and how its answer becomes the
//! response. The ownership rules (own sessions only, the same 404 for another user's id) are the
//! adapter's and are proved against a real server in `spot-cli`'s `tests/session.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::ports::session::{SessionOut, SessionPort, SessionResult};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{RouteState, session};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const TOKEN: &str = "tok-alice";

#[derive(Default)]
struct Script {
    /// The next call fails with this.
    fail: Option<ApiError>,
    /// What `list` answers.
    list: Vec<SessionOut>,
    /// Calls the port received: `(operation, id)`.
    calls: Vec<(&'static str, String)>,
    /// The credential token of each call.
    tokens: Vec<String>,
}

struct Fake {
    script: Mutex<Script>,
}

impl Fake {
    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn record(&self, ctx: &RequestCtx, op: &'static str, id: String) -> SessionResult<()> {
        let mut script = self.lock();
        script.tokens.push(
            ctx.credential
                .as_ref()
                .map(|c| c.token.clone())
                .unwrap_or_default(),
        );
        script.calls.push((op, id));
        match script.fail.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl SessionPort for Fake {
    fn list<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, SessionResult<Vec<SessionOut>>> {
        Box::pin(async move {
            self.record(ctx, "list", String::new())?;
            Ok(self.lock().list.clone())
        })
    }

    fn end<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str) -> BoxFuture<'a, SessionResult<()>> {
        Box::pin(async move { self.record(ctx, "end", id.to_owned()) })
    }
}

fn session_out(id: &str, current: bool) -> SessionOut {
    SessionOut {
        id: id.to_owned(),
        created_at: "2026-10-09T18:04:11Z".to_owned(),
        expires_at: "2026-10-10T18:04:11Z".to_owned(),
        last_used_at: Some("2026-10-10T09:41:02Z".to_owned()),
        method: "password".to_owned(),
        source: "203.0.113.7".to_owned(),
        device: Some("Ayesha's laptop".to_owned()),
        current,
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
    ports.provide::<dyn SessionPort>(fake.clone());
    let state = RouteState::new(ports.subset(session::session_required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(session::session_routes().with_state(state))
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
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn send(f: &Fixture, method: &str, path: &str, token: Option<&str>) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).unwrap();
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

async fn list(f: &Fixture) -> Reply {
    send(f, "GET", "/api/v1/auth/sessions", Some(TOKEN)).await
}

async fn end(f: &Fixture, id: &str) -> Reply {
    send(
        f,
        "DELETE",
        &format!("/api/v1/auth/sessions/{id}"),
        Some(TOKEN),
    )
    .await
}

fn fail_next(f: &Fixture, error: ApiError) {
    f.fake.lock().fail = Some(error);
}

fn calls(f: &Fixture) -> Vec<(&'static str, String)> {
    f.fake.lock().calls.clone()
}

// ------------------------------------------------------------------------------------------
// auth_session_list

#[tokio::test]
async fn list_answers_the_contract_record_in_the_port_order() {
    let f = fixture();
    f.fake.lock().list = vec![session_out("cur_1", true), session_out("old-2", false)];
    let r = list(&f).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert!(r.header("x-request-id").is_some());
    let body = r.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(
        data[0],
        json!({
            "id": "cur_1",
            "created_at": "2026-10-09T18:04:11Z",
            "expires_at": "2026-10-10T18:04:11Z",
            "last_used_at": "2026-10-10T09:41:02Z",
            "method": "password",
            "source": "203.0.113.7",
            "device": "Ayesha's laptop",
            "current": true
        })
    );
    assert_eq!(data[1]["id"], "old-2");
    assert_eq!(data[1]["current"], false);
    assert_eq!(f.fake.lock().tokens, vec![TOKEN.to_owned()]);
}

#[tokio::test]
async fn optional_fields_are_left_out_when_unknown_and_nothing_else_is_added() {
    let f = fixture();
    let mut s = session_out("cur_1", true);
    s.last_used_at = None;
    s.device = None;
    f.fake.lock().list = vec![s];
    let r = list(&f).await;
    let record = r.json()["data"][0].clone();
    let mut keys: Vec<&str> = record
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "created_at",
            "current",
            "expires_at",
            "id",
            "method",
            "source"
        ]
    );
    let text = r.text();
    for forbidden in ["token", "digest", "hash", "secret"] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
}

#[tokio::test]
async fn an_empty_list_is_an_empty_array_not_an_error() {
    let f = fixture();
    let r = list(&f).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), json!({"data": []}));
}

#[tokio::test]
async fn list_refuses_a_request_without_a_credential_before_the_port() {
    let f = fixture();
    let r = send(&f, "GET", "/api/v1/auth/sessions", None).await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(r.code(), "auth.required");
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert!(calls(&f).is_empty(), "the port is not asked");
}

#[tokio::test]
async fn list_relays_the_ports_refusals_with_their_codes() {
    for (error, status, code) in [
        (
            ApiError::from_code(Code::AuthRequired),
            401,
            "auth.required",
        ),
        (
            ApiError::from_code(Code::AuthSessionExpired),
            401,
            "auth.session_expired",
        ),
        // An API key bearer is refused by the adapter with this.
        (
            ApiError::from_code(Code::PermissionDenied),
            403,
            "permission.denied",
        ),
        (
            ApiError::from_code(Code::ServerUnavailable),
            503,
            "server.unavailable",
        ),
    ] {
        let f = fixture();
        fail_next(&f, error);
        let r = list(&f).await;
        assert_eq!(r.status, status, "{}", r.text());
        assert_eq!(r.code(), code);
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
    }
}

#[tokio::test]
async fn list_passes_the_port_no_user_and_ignores_a_query() {
    let f = fixture();
    let r = send(
        &f,
        "GET",
        "/api/v1/auth/sessions?user=someone-else&all=true",
        Some(TOKEN),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(calls(&f), vec![("list", String::new())]);
}

// ------------------------------------------------------------------------------------------
// auth_session_end

#[tokio::test]
async fn end_answers_204_with_no_body_and_passes_the_id_to_the_port() {
    let f = fixture();
    let r = end(&f, "cur_1").await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(r.bytes.is_empty());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert!(r.header("x-request-id").is_some());
    assert_eq!(calls(&f), vec![("end", "cur_1".to_owned())]);
    assert_eq!(f.fake.lock().tokens, vec![TOKEN.to_owned()]);
}

#[tokio::test]
async fn end_relays_the_ports_not_found_and_other_refusals() {
    for (error, status, code) in [
        (
            ApiError::from_code(Code::AuthSessionNotFound),
            404,
            "auth.session_not_found",
        ),
        (
            ApiError::from_code(Code::AuthRequired),
            401,
            "auth.required",
        ),
        (
            ApiError::from_code(Code::PermissionDenied),
            403,
            "permission.denied",
        ),
        (
            ApiError::from_code(Code::ServerUnavailable),
            503,
            "server.unavailable",
        ),
    ] {
        let f = fixture();
        fail_next(&f, error);
        let r = end(&f, "abc").await;
        assert_eq!(r.status, status, "{}", r.text());
        assert_eq!(r.code(), code);
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
    }
}

#[tokio::test]
async fn an_unknown_and_a_foreign_id_are_the_same_answer_to_the_byte() {
    // The routes cannot tell the two apart (the port answers both with one code), so what the
    // caller sees is identical apart from the request id.
    let strip = |r: &Reply| {
        let mut body = r.json();
        body["error"]["request_id"] = Json::Null;
        (r.status, body, r.header("cache-control").map(str::to_owned))
    };
    let f = fixture();
    fail_next(&f, ApiError::from_code(Code::AuthSessionNotFound));
    let unknown = end(&f, "never-existed").await;
    fail_next(&f, ApiError::from_code(Code::AuthSessionNotFound));
    let foreign = end(&f, "someone-elses-id").await;
    assert_eq!(strip(&unknown), strip(&foreign));
    assert_eq!(unknown.status, 404);
}

#[tokio::test]
async fn end_refuses_a_request_without_a_credential_before_the_port() {
    let f = fixture();
    let r = send(&f, "DELETE", "/api/v1/auth/sessions/abc", None).await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(r.code(), "auth.required");
    assert!(calls(&f).is_empty());
}

#[tokio::test]
async fn end_refuses_an_id_of_the_wrong_shape_before_the_port() {
    let f = fixture();
    let too_long = "a".repeat(65);
    for id in [
        "has.dot",
        "has%20space",
        "semi;colon",
        "%2e%2e",
        "caf%C3%A9",
        too_long.as_str(),
    ] {
        let r = end(&f, id).await;
        assert_eq!(r.status, 400, "{id}: {}", r.text());
        assert_eq!(r.code(), "api.invalid_request", "{id}");
    }
    assert!(
        calls(&f).is_empty(),
        "the port is never asked about a malformed id"
    );
}

#[tokio::test]
async fn end_accepts_the_longest_and_the_hyphen_underscore_ids() {
    let f = fixture();
    let longest = "a".repeat(64);
    for id in [
        longest.as_str(),
        "a-b_c",
        "0192f3c4-5b6a-7c8d-9e0f-00000000se01",
    ] {
        let r = end(&f, id).await;
        assert_eq!(r.status, 204, "{id}: {}", r.text());
    }
    assert_eq!(calls(&f).len(), 3);
}

#[tokio::test]
async fn the_wrong_methods_are_refused_before_the_port() {
    let f = fixture();
    for (method, path) in [
        ("POST", "/api/v1/auth/sessions"),
        ("PUT", "/api/v1/auth/sessions"),
        ("DELETE", "/api/v1/auth/sessions"),
        ("GET", "/api/v1/auth/sessions/abc"),
        ("PATCH", "/api/v1/auth/sessions/abc"),
        ("POST", "/api/v1/auth/sessions/abc"),
    ] {
        let r = send(&f, method, path, Some(TOKEN)).await;
        assert_eq!(r.status, 405, "{method} {path}");
    }
    assert!(calls(&f).is_empty());
}

#[test]
fn the_group_declares_its_operations_and_needs_the_session_port() {
    assert_eq!(
        session::SESSION_SERVES,
        &["auth_session_list", "auth_session_end"]
    );
    assert_eq!(session::session_required_ports().len(), 1);
    let registration = session::registration();
    assert_eq!(registration.name, "session");
    assert!(
        registration.public.is_none(),
        "the group is authenticated, never PUBLIC"
    );
}
