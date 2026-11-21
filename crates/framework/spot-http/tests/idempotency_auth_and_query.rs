//! The `Idempotency-Key` layer and the caller's standing (WP-PLT-05.T11): a `401` and a `403`
//! are released (never stored), a `404` is stored and replayed, and the query string is part of
//! the request fingerprint.
//!
//! A request that was in flight at deploy time under the old fingerprint (method, path, body)
//! answers `409 api.idempotency_conflict` to its own retry once, because a key lives 24 hours;
//! this is accepted and not tested here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)]
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, header};
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Code;
use spot_http::api::codec::{ApiDocument, DocumentPatchApi};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::idempotency_contract::{RecordingStore, StoreCall};
use spot_http::api::router::{ApiServices, router};
use spot_http::api::service::{
    ApiError, BoxFuture, Credential, Message, MethodOutcome, Page, QuerySpec, ReportResult,
    RequestCtx, ResourceService, WriteResult,
};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_storage_contract::{RecordId, Version};
use support::fake::{FakeApp, TOKEN_ALICE};
use time::OffsetDateTime;
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const GOOD: &str = "good-token";

/// What `create` does.
enum Mode {
    /// `good-token` creates (as the fake's real user); any other bearer is `401`.
    TokenGate,
    /// Every create answers this error.
    Fixed(Code),
}

/// A resource service that answers the way the test says for `create` and delegates the rest to
/// the fake. It counts the creates that reach it.
struct Gate {
    inner: Arc<FakeApp>,
    mode: Mutex<Mode>,
    creates: AtomicUsize,
}

impl ResourceService for Gate {
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        ResourceService::get(&*self.inner, ctx, doctype, id)
    }
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        ResourceService::list(&*self.inner, ctx, doctype, spec)
    }
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        ResourceService::query(&*self.inner, ctx, doctype, spec)
    }
    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        document: &'a ApiDocument,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            self.creates.fetch_add(1, Ordering::SeqCst);
            let outcome = match &*self.mode.lock().unwrap() {
                Mode::Fixed(code) => Some(ApiError::from_code(*code)),
                Mode::TokenGate => {
                    let good = ctx.credential.as_ref().is_some_and(|c| c.token == GOOD);
                    (!good).then(|| ApiError::from_code(Code::AuthInvalidCredentials))
                }
            };
            if let Some(error) = outcome {
                return Err(error);
            }
            // The gate let the caller through: create as the fake's own user.
            let mut as_alice = ctx.clone();
            as_alice.credential = Some(Credential::new(TOKEN_ALICE));
            ResourceService::create(&*self.inner, &as_alice, doctype, document).await
        })
    }
    fn update<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        patch: &'a DocumentPatchApi,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        ResourceService::update(&*self.inner, ctx, doctype, id, patch, expected)
    }
    fn delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<Vec<Message>, ApiError>> {
        ResourceService::delete(&*self.inner, ctx, doctype, id, expected)
    }
    fn post<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        ResourceService::post(&*self.inner, ctx, doctype, id, expected)
    }
    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        ResourceService::cancel(&*self.inner, ctx, doctype, id, expected)
    }
    fn correct<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        ResourceService::correct(&*self.inner, ctx, doctype, id, expected)
    }
    fn rename<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        ResourceService::rename(&*self.inner, ctx, doctype, id, new_name, expected)
    }
    fn call_method<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, Result<(MethodOutcome, Vec<Message>), ApiError>> {
        ResourceService::call_method(&*self.inner, ctx, doctype, id, method, args, expected)
    }
    fn call_function<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<Message>), ApiError>> {
        ResourceService::call_function(&*self.inner, ctx, app, module, function, args, via_get)
    }
    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>> {
        ResourceService::run_report(&*self.inner, ctx, report, filters, limit)
    }
}

struct Fixture {
    router: axum::Router,
    gate: Arc<Gate>,
    store: Arc<RecordingStore>,
}

fn fixture(mode: Mode) -> Fixture {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
    let fake = FakeApp::new(clock.clone());
    let store = Arc::new(RecordingStore::new(Arc::new(MemoryIdempotencyStore::new(
        clock,
    ))));
    let gate = Arc::new(Gate {
        inner: fake.clone(),
        mode: Mutex::new(mode),
        creates: AtomicUsize::new(0),
    });
    let services = ApiServices::new(
        fake.clone(),
        gate.clone(),
        fake.clone(),
        fake.clone(),
        store.clone(),
    );
    let site = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build();
    Fixture {
        router,
        gate,
        store,
    }
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: Json,
}

impl Reply {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
    fn replayed(&self) -> bool {
        self.headers.get("idempotency-replayed").is_some()
    }
}

async fn create(f: &Fixture, path: &str, token: &str, key: &str) -> Reply {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("idempotency-key", key)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"name": "ACME"}).to_string()))
        .unwrap();
    let response = f.router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Json::Null),
    }
}

const PATH: &str = "/api/v1/resource/customer";

fn completes(f: &Fixture) -> usize {
    f.store
        .calls()
        .iter()
        .filter(|call| matches!(call, StoreCall::Complete(_)))
        .count()
}

#[tokio::test]
async fn a_401_from_the_service_is_never_stored_and_never_replayed() {
    let f = fixture(Mode::TokenGate);
    for n in 1..=2 {
        let r = create(&f, PATH, "bogus-token", "k-401").await;
        assert_eq!(r.status, 401, "attempt {n}: {}", r.body);
        assert!(
            !r.replayed(),
            "attempt {n}: a rejected token is never a replay"
        );
    }
    assert_eq!(
        f.gate.creates.load(Ordering::SeqCst),
        2,
        "both reached the service"
    );
    assert_eq!(completes(&f), 0, "no durable row for a rejected token");
    // The same key with the good token runs fresh and is a live 201.
    let r = create(&f, PATH, GOOD, "k-401").await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(!r.replayed());
    assert_eq!(f.gate.creates.load(Ordering::SeqCst), 3);
    assert_eq!(completes(&f), 1, "only the success is stored");
}

#[tokio::test]
async fn a_403_is_released_like_a_401_and_a_404_is_stored_and_replayed() {
    let f = fixture(Mode::Fixed(Code::PermissionDenied));
    for n in 1..=2 {
        let r = create(&f, PATH, GOOD, "k-403").await;
        assert_eq!(
            (r.status, r.code()),
            (403, "permission.denied"),
            "attempt {n}"
        );
        assert!(!r.replayed(), "attempt {n}");
    }
    assert_eq!(
        f.gate.creates.load(Ordering::SeqCst),
        2,
        "a 403 retry reaches the service"
    );
    assert_eq!(completes(&f), 0, "a 403 is never kept for 24 hours");
    // After the right is granted a retry with the same key and body runs.
    *f.gate.mode.lock().unwrap() = Mode::TokenGate;
    let r = create(&f, PATH, GOOD, "k-403").await;
    assert_eq!(r.status, 201, "{}", r.body);
    // The release set was not widened by accident: a 404 is stored and replayed.
    *f.gate.mode.lock().unwrap() = Mode::Fixed(Code::DocumentNotFound);
    let before = f.gate.creates.load(Ordering::SeqCst);
    let first = create(&f, PATH, GOOD, "k-404").await;
    assert_eq!(first.status, 404, "{}", first.body);
    assert!(!first.replayed());
    let second = create(&f, PATH, GOOD, "k-404").await;
    assert_eq!(second.status, 404);
    assert!(second.replayed(), "the 404 is replayed from the store");
    assert_eq!(
        f.gate.creates.load(Ordering::SeqCst),
        before + 1,
        "the replay did not reach the service"
    );
}

#[tokio::test]
async fn the_query_string_is_part_of_the_fingerprint() {
    let f = fixture(Mode::TokenGate);
    let first = create(&f, &format!("{PATH}?a=1"), GOOD, "k-q").await;
    assert_eq!(first.status, 201, "{}", first.body);
    // (c) the same key, body and path with another query is a different request: a conflict.
    let other = create(&f, &format!("{PATH}?a=2"), GOOD, "k-q").await;
    assert_eq!(
        (other.status, other.code()),
        (409, "api.idempotency_conflict")
    );
    // The same query twice replays.
    let same = create(&f, &format!("{PATH}?a=1"), GOOD, "k-q").await;
    assert_eq!(same.status, 201);
    assert!(same.replayed());
    // (d) a request with the query and the same request without it conflict too, both ways.
    let bare = create(&f, PATH, GOOD, "k-q").await;
    assert_eq!(
        (bare.status, bare.code()),
        (409, "api.idempotency_conflict")
    );
    let fresh = create(&f, PATH, GOOD, "k-bare").await;
    assert_eq!(fresh.status, 201);
    let with_query = create(&f, &format!("{PATH}?a=1"), GOOD, "k-bare").await;
    assert_eq!(with_query.code(), "api.idempotency_conflict");
    // The raw query is compared as received: another order is another request.
    let ab = create(&f, &format!("{PATH}?a=1&b=2"), GOOD, "k-order").await;
    assert_eq!(ab.status, 201);
    let ba = create(&f, &format!("{PATH}?b=2&a=1"), GOOD, "k-order").await;
    assert_eq!(ba.code(), "api.idempotency_conflict");
}
