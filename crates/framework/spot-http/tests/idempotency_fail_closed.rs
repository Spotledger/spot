//! The `Idempotency-Key` store fails closed (WP-PLT-05.T7, ADR-082): when the store is
//! down no keyed command runs and the client gets `503 server.unavailable` with `Retry-After`;
//! routes that can carry a secret make no store call at all; a route whose response is app
//! code's (`method`, `function`) reserves and then releases and never writes a response; an
//! undeclared route in the commands group fails the suite.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)]
mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::idempotency::{
    IdempotencyError, IdempotencyStore, KeyScope, MemoryIdempotencyStore, Reservation, StoreFuture,
    StoredResponse,
};
use spot_http::api::idempotency_contract::{FailingStore, RecordingStore, StoreCall};
use spot_http::api::ports::consumer_sessions::{ConsumerSessions, MemoryConsumerSessions};
use spot_http::api::router::routes::{RouteState, consumer_renew};
use spot_http::api::router::{ApiServices, router};
use spot_http::api::{MemoryFilesPort, Ports};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use support::Must;
use support::fake::{FakeApp, TOKEN_ALICE};
use support::harness::{self, App, HOST, SITE};
use support::permissions::{FakePermissions, FakeReveal};
use time::OffsetDateTime;
use tower::ServiceExt as _;

/// The router over the fake services with `store` as the idempotency store, and the consumer
/// renewal route merged in (a public route, outside the bearer layer).
fn app_with(store: Arc<dyn IdempotencyStore>) -> App {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
    let fake = FakeApp::new(clock.clone());
    let permissions = Arc::new(FakePermissions::new());
    let reveal = Arc::new(FakeReveal::new());
    let site = SiteId::new(SITE).must();
    let services = ApiServices::new(
        fake.clone(),
        fake.clone(),
        fake.clone(),
        fake.clone(),
        store,
    )
    .with_permissions(permissions.clone())
    .with_reveal(reveal.clone())
    .with_files(Arc::new(MemoryFilesPort::new(1024)));
    let sessions = Arc::new(MemoryConsumerSessions::new(
        clock.clone(),
        "test-seed",
        time::Duration::minutes(15),
    ));
    let mut ports = Ports::new();
    ports.provide::<dyn ConsumerSessions>(sessions);
    let state = RouteState::new(ports.subset(consumer_renew::required_ports()).must());
    let api = router(services).merge(consumer_renew::routes().with_state(state));
    let server_router = ServerBuilder::new(
        ServerConfig {
            max_body_bytes: 64 * 1024,
            ..ServerConfig::default()
        },
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(api)
    .build();
    App {
        router: server_router,
        fake,
        permissions,
        reveal,
        // Unused by these tests; the harness type wants a memory store.
        store: Arc::new(MemoryIdempotencyStore::new(clock.clone())),
        clock,
    }
}

fn recording() -> (App, Arc<RecordingStore>) {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
    let store = Arc::new(RecordingStore::new(Arc::new(MemoryIdempotencyStore::new(
        clock,
    ))));
    (app_with(store.clone()), store)
}

fn failing() -> (App, Arc<FailingStore>) {
    let store = Arc::new(FailingStore::new(3));
    (app_with(store.clone()), store)
}

const ID: &str = "01970000-0000-7000-8000-0000000000aa";

/// Every route of the commands group with a request body that passes the middleware's checks.
fn command_requests() -> Vec<(&'static str, String, Option<Json>)> {
    vec![
        (
            "POST",
            "/api/v1/resource/customer".to_owned(),
            Some(json!({ "name": "ACME" })),
        ),
        (
            "POST",
            format!("/api/v1/resource/sales_invoice/{ID}/post"),
            None,
        ),
        (
            "POST",
            format!("/api/v1/resource/sales_invoice/{ID}/cancel"),
            Some(json!({ "reason": "x" })),
        ),
        (
            "POST",
            format!("/api/v1/resource/sales_invoice/{ID}/correct"),
            Some(json!({ "reason": "x" })),
        ),
        (
            "POST",
            format!("/api/v1/resource/sales_invoice/{ID}/rename"),
            Some(json!({ "new_id": "x" })),
        ),
        (
            "POST",
            format!("/api/v1/resource/sales_invoice/{ID}/methods/totals"),
            None,
        ),
        (
            "POST",
            "/api/v1/method/spot/sales/fn".to_owned(),
            Some(json!({ "args": {} })),
        ),
    ]
}

#[tokio::test]
async fn a_downed_store_refuses_every_keyed_command_and_runs_none() {
    let (app, store) = failing();
    let mut seen = 0;
    for (method, path, body) in command_requests() {
        let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("down-1")];
        let call = harness::call(&app, method, &path, &head, body).await;
        call.expect_status(503, &format!("{path} with the store down"));
        assert_eq!(call.error_code(), "server.unavailable", "{path}");
        assert_eq!(
            call.header("retry-after"),
            Some("3"),
            "{path}: Retry-After is the store's hint"
        );
        assert!(
            call.header("idempotency-replayed").is_none(),
            "{path}: a refusal is never a replay"
        );
        seen += 1;
    }
    assert_eq!(seen, 7);
    assert_eq!(
        app.fake.stored_count_of("customer"),
        0,
        "the create did not run"
    );
    let calls = store.calls();
    assert!(
        calls.iter().all(|call| *call == StoreCall::Reserve),
        "the only store call is the refused reserve: {calls:?}"
    );
    assert_eq!(calls.len(), 7, "one reserve per request, no retry loop");
}

#[tokio::test]
async fn a_downed_store_refuses_the_upload_and_stores_no_file() {
    let (app, store) = failing();
    let response = app
        .router
        .clone()
        .oneshot(upload_request("down-upload", b"hello"))
        .await
        .must();
    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(response.headers().get("retry-after").must(), "3");
    assert_eq!(store.calls(), vec![StoreCall::Reserve]);
}

#[tokio::test]
async fn a_request_without_a_key_does_not_touch_the_store() {
    let (app, store) = failing();
    let call = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &[harness::bearer(TOKEN_ALICE)],
        Some(json!({ "name": "ACME no key" })),
    )
    .await;
    call.expect_status(201, "no key, no idempotency, no store");
    assert!(store.calls().is_empty());
}

#[tokio::test]
async fn the_routes_that_can_carry_a_secret_make_no_store_call() {
    let (app, store) = recording();
    let key = harness::idem_key("secret-route-1");
    let bearer = harness::bearer(TOKEN_ALICE);
    // login (unauthenticated), session, logout, reveal, consumer renew.
    let login = harness::call(
        &app,
        "POST",
        "/api/v1/auth/login",
        std::slice::from_ref(&key),
        Some(json!({ "user": "alice@acme.com", "password": "correct horse" })),
    )
    .await;
    assert!(login.status < 500, "login answered: {}", login.raw);
    harness::call(
        &app,
        "GET",
        "/api/v1/auth/session",
        &[bearer.clone(), key.clone()],
        None,
    )
    .await;
    harness::call(
        &app,
        "POST",
        "/api/v1/auth/logout",
        &[bearer.clone(), key.clone()],
        None,
    )
    .await;
    harness::call(
        &app,
        "POST",
        &format!("/api/v1/resource/customer/{ID}/reveal"),
        &[bearer.clone(), key.clone()],
        Some(json!({ "field": "phone", "reason": "Customer asked on ticket 1." })),
    )
    .await;
    let renew = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/consumer/renew")
                .header(header::HOST, HOST)
                .header("idempotency-key", "secret-route-1")
                .body(Body::empty())
                .must(),
        )
        .await
        .must();
    assert_eq!(renew.status().as_u16(), 401, "no cookie, no renewal");
    assert!(
        store.calls().is_empty(),
        "login, session, logout, reveal and consumer renew touch the store zero times: {:?}",
        store.calls()
    );
}

#[tokio::test]
async fn a_response_that_is_app_codes_is_reserved_then_released_never_stored() {
    let (app, store) = recording();
    for (path, body) in [
        (
            format!("/api/v1/resource/sales_invoice/{ID}/methods/totals"),
            None,
        ),
        (
            "/api/v1/method/spot/sales/fn".to_owned(),
            Some(json!({ "args": {} })),
        ),
    ] {
        store.clear();
        let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("none-1")];
        harness::call(&app, "POST", &path, &head, body).await;
        assert_eq!(
            store.calls(),
            vec![StoreCall::Reserve, StoreCall::Fail],
            "{path}: reserve while it runs, release at the end, never complete"
        );
    }
}

#[tokio::test]
async fn a_lifecycle_response_is_stored_under_the_http_api_class() {
    let (app, store) = recording();
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("body-1")];
    let first = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &head,
        Some(json!({ "name": "ACME stored" })),
    )
    .await;
    first.expect_status(201, "create");
    assert_eq!(
        store.calls(),
        vec![
            StoreCall::Reserve,
            StoreCall::Complete("http_api".to_owned())
        ]
    );
    store.clear();
    let second = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &head,
        Some(json!({ "name": "ACME stored" })),
    )
    .await;
    assert_eq!(second.header("idempotency-replayed"), Some("true"));
    assert_eq!(second.raw, first.raw);
    assert_eq!(store.calls(), vec![StoreCall::Reserve]);
}

#[tokio::test]
async fn the_upload_is_stored_under_the_http_upload_class() {
    let (app, store) = recording();
    let response = app
        .router
        .clone()
        .oneshot(upload_request("upload-1", b"hello"))
        .await
        .must();
    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(
        store.calls(),
        vec![
            StoreCall::Reserve,
            StoreCall::Complete("http_upload".to_owned())
        ]
    );
}

#[tokio::test]
async fn a_body_larger_than_the_store_keeps_is_released_not_stored() {
    let (app, store) = recording();
    store.set_max_body_bytes(8);
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("big-1")];
    let first = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &head,
        Some(json!({ "name": "ACME big" })),
    )
    .await;
    first.expect_status(201, "the command still answers");
    assert_eq!(
        store.calls(),
        vec![StoreCall::Reserve, StoreCall::Fail],
        "released, nothing written"
    );
    // The key is free: a retry runs again.
    let second = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &head,
        Some(json!({ "name": "ACME big" })),
    )
    .await;
    second.expect_status(201, "a retry runs fresh");
    assert!(second.header("idempotency-replayed").is_none());
    assert_eq!(app.fake.stored_count_of("customer"), 2);
}

#[tokio::test]
async fn a_complete_that_fails_after_the_command_ran_still_returns_the_real_response() {
    let (app, store) = recording();
    store.set_complete_fails(true);
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("late-1")];
    let call = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &head,
        Some(json!({ "name": "ACME late" })),
    )
    .await;
    call.expect_status(201, "the command ran; its response is returned");
    assert_eq!(app.fake.stored_count_of("customer"), 1);
    assert_eq!(
        store.calls(),
        vec![
            StoreCall::Reserve,
            StoreCall::Complete("http_api".to_owned())
        ],
        "no release after a failed complete: the lease frees the key, a retry is not a lie"
    );
}

/// A store whose key is always pending and whose wait budget always runs out.
struct StuckPending;

impl IdempotencyStore for StuckPending {
    fn reserve<'a>(
        &'a self,
        _scope: &'a KeyScope,
        _fingerprint: &'a str,
    ) -> StoreFuture<'a, Result<Reservation, IdempotencyError>> {
        Box::pin(std::future::ready(Ok(Reservation::Pending)))
    }

    fn complete<'a>(
        &'a self,
        _scope: &'a KeyScope,
        _fingerprint: &'a str,
        _response: StoredResponse,
        _class: &'a str,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn fail<'a>(&'a self, _scope: &'a KeyScope, _fingerprint: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }

    fn wait_pending<'a>(
        &'a self,
        _scope: &'a KeyScope,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        Box::pin(std::future::ready(Err(IdempotencyError::unavailable())))
    }
}

/// A store that says "resolved" from every wait yet still finds the key pending: the router
/// must stop after a bounded number of rounds, not spin.
struct NeverResolves;

impl IdempotencyStore for NeverResolves {
    fn reserve<'a>(
        &'a self,
        _scope: &'a KeyScope,
        _fingerprint: &'a str,
    ) -> StoreFuture<'a, Result<Reservation, IdempotencyError>> {
        Box::pin(std::future::ready(Ok(Reservation::Pending)))
    }

    fn complete<'a>(
        &'a self,
        _scope: &'a KeyScope,
        _fingerprint: &'a str,
        _response: StoredResponse,
        _class: &'a str,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn fail<'a>(&'a self, _scope: &'a KeyScope, _fingerprint: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }

    fn wait_pending<'a>(
        &'a self,
        _scope: &'a KeyScope,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

#[tokio::test]
async fn a_duplicate_whose_wait_budget_ran_out_is_refused_and_never_run() {
    let app = app_with(Arc::new(StuckPending));
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("wait-1")];
    let call = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &head,
        Some(json!({ "name": "ACME wait" })),
    )
    .await;
    call.expect_status(503, "the wait ran out: retryable 503, never 409");
    assert_eq!(call.error_code(), "server.unavailable");
    assert_eq!(call.header("retry-after"), Some("1"));
    assert_eq!(app.fake.stored_count_of("customer"), 0, "never run");
}

#[tokio::test]
async fn a_key_that_stays_pending_forever_ends_in_503_not_a_spin() {
    let app = app_with(Arc::new(NeverResolves));
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("spin-1")];
    let call = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        harness::call(
            &app,
            "POST",
            "/api/v1/resource/customer",
            &head,
            Some(json!({ "name": "ACME spin" })),
        ),
    )
    .await
    .must();
    call.expect_status(503, "bounded rounds, then refused");
    assert_eq!(app.fake.stored_count_of("customer"), 0, "never run");
}

/// A multipart-free upload: raw octets with a `Content-Disposition` file name.
fn upload_request(key: &str, bytes: &[u8]) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/files")
        .header(header::HOST, HOST)
        .header("authorization", format!("Bearer {TOKEN_ALICE}"))
        .header("idempotency-key", key)
        .header("content-type", "application/octet-stream")
        .header("content-disposition", "attachment; filename=\"a.txt\"")
        .body(Body::from(bytes.to_vec()))
        .must()
}

/// The paths of the commands group, read from the router source: the `Router::new()` that
/// ends in `idempotency_middleware`.
fn commands_group_paths() -> Vec<String> {
    let source = include_str!("../src/api/router/mod.rs");
    let start = source
        .find("let commands = Router::new()")
        .expect("the commands group is declared in router/mod.rs");
    let end = source[start..]
        .find("middleware::idempotency_middleware")
        .expect("the commands group carries the idempotency layer")
        + start;
    let mut paths = Vec::new();
    let mut rest = &source[start..end];
    while let Some(at) = rest.find("\"/api/v1/") {
        let tail = &rest[at + 1..];
        let close = tail.find('"').expect("a closed string");
        paths.push(tail[..close].to_owned());
        rest = &tail[close + 1..];
    }
    paths.sort();
    paths
}

/// What each route of the commands group may persist. A route added to the group must be added
/// here, with its persistence declared (and `middleware::COMMAND_ROUTES` kept in step: the
/// behaviour tests above fail when a `Persist::None` route stores a body).
const DECLARED: [(&str, &str); 7] = [
    ("/api/v1/resource/{doctype}", "body"),
    ("/api/v1/resource/{doctype}/{id}/post", "body"),
    ("/api/v1/resource/{doctype}/{id}/cancel", "body"),
    ("/api/v1/resource/{doctype}/{id}/correct", "body"),
    ("/api/v1/resource/{doctype}/{id}/rename", "body"),
    ("/api/v1/resource/{doctype}/{id}/methods/{method}", "none"),
    ("/api/v1/method/{app}/{module}/{function}", "none"),
];

#[test]
fn every_route_of_the_commands_group_declares_its_persistence() {
    let mut declared: Vec<String> = DECLARED
        .iter()
        .map(|(path, _)| (*path).to_owned())
        .collect();
    declared.sort();
    declared.dedup();
    assert_eq!(
        commands_group_paths(),
        declared,
        "a route was added to (or removed from) the commands group: declare what it may persist \
         in DECLARED, and in middleware::COMMAND_ROUTES (an unlisted route persists nothing)"
    );
}

/// The behaviour that the declaration stands for: only the `body` routes ever complete.
#[tokio::test]
async fn only_the_declared_body_routes_ever_complete() {
    let (app, store) = recording();
    for (n, (method, path, body)) in command_requests().into_iter().enumerate() {
        store.clear();
        let head = [
            harness::bearer(TOKEN_ALICE),
            harness::idem_key(&format!("declared-{n}")),
        ];
        harness::call(&app, method, &path, &head, body).await;
        let completed = store
            .calls()
            .iter()
            .any(|call| matches!(call, StoreCall::Complete(_)));
        let wants_body = DECLARED
            .iter()
            .any(|(pattern, kind)| *kind == "body" && matches_pattern(pattern, &path));
        let none_route = DECLARED
            .iter()
            .any(|(pattern, kind)| *kind == "none" && matches_pattern(pattern, &path));
        if none_route {
            assert!(!completed, "{path} wrote a response: {:?}", store.calls());
        }
        if wants_body {
            // A body route completes unless the handler's status releases the key.
            let released = store.calls().contains(&StoreCall::Fail);
            assert!(completed || released, "{path}: {:?}", store.calls());
        }
    }
}

/// Whether `path` is an instance of `pattern` (`{x}` matches one segment).
fn matches_pattern(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let q: Vec<&str> = path.split('/').collect();
    p.len() == q.len()
        && p.iter()
            .zip(&q)
            .all(|(a, b)| (a.starts_with('{') && a.ends_with('}')) || a == b)
}
