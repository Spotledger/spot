//! `Retry-After` is rendered from the error for every code that carries a wait, and a storage
//! `Unavailable` reaches the client as `503 server.unavailable` through the resource, lookup,
//! file and sign-in routes, with no connection detail in the body.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs, dead_code)]

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, header};
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::codec::{ApiDocument, DocumentPatchApi};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::router::{ApiServices, router};
use spot_http::api::service::{
    ApiError, AuthService, BoxFuture, LoginBody, LookupService, Page, QuerySpec, RequestCtx,
    ResourceService, Session, SessionInfo, WriteResult,
};
use spot_http::api::{
    BoxByteStream, Code, FileDownload, FileGetRequest, FilePortError, FileRecord, FileUpload,
    FilesPort, LookupPage, LookupRequest, storage_error_to_api,
};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_storage_contract::{ErrorKind, RecordId, StorageError, Version};
use support::Must;
use support::fake::{FakeApp, TOKEN_ALICE};
use time::OffsetDateTime;
use tower::ServiceExt;

const HOST: &str = "erp.acme.com";
const ID: &str = "01900000-0000-7000-8000-000000000001";
const LEAKS: [&str; 7] = [
    "postgres",
    "hunter2",
    "db-7",
    "5432",
    "pool wait",
    "max 32",
    "5s",
];

fn storage_unavailable() -> StorageError {
    StorageError::new(
        ErrorKind::Unavailable,
        "pool wait of 5s exceeded for postgres://app:hunter2@db-7.internal:5432/spot (max 32)",
    )
}

/// Services that all fail with the error they were built with.
struct Down(ApiError);

impl ResourceService for Down {
    fn get<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RecordId,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn list<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn query<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn create<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a ApiDocument,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn update<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RecordId,
        _: &'a DocumentPatchApi,
        _: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn delete<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RecordId,
        _: Version,
    ) -> BoxFuture<'a, Result<Vec<spot_http::api::Message>, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn post<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RecordId,
        _: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn cancel<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RecordId,
        _: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn correct<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RecordId,
        _: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn rename<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RecordId,
        _: &'a str,
        _: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn call_method<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: RecordId,
        _: &'a str,
        _: Json,
        _: Option<Version>,
    ) -> BoxFuture<
        'a,
        Result<
            (
                spot_http::api::service::MethodOutcome,
                Vec<spot_http::api::Message>,
            ),
            ApiError,
        >,
    > {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn call_function<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
        _: &'a str,
        _: Json,
        _: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<spot_http::api::Message>), ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn run_report<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: Json,
        _: Option<u64>,
    ) -> BoxFuture<
        'a,
        Result<
            (
                spot_http::api::service::ReportResult,
                Vec<spot_http::api::Message>,
            ),
            ApiError,
        >,
    > {
        Box::pin(async move { Err(self.0.clone()) })
    }
}

impl LookupService for Down {
    fn search<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
        _: &'a LookupRequest,
    ) -> BoxFuture<'a, Result<LookupPage, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
}

impl AuthService for Down {
    fn login<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn logout<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
    fn session<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
}

/// A file store whose every call fails the way a port reports `server.unavailable`: through
/// `FilePortError::from(ApiError)`, which keeps the wait.
struct DownFiles(ApiError);

impl FilesPort for DownFiles {
    fn max_upload_bytes(&self) -> u64 {
        1024
    }
    fn upload<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: FileUpload,
        _: BoxByteStream,
    ) -> BoxFuture<'a, Result<FileRecord, FilePortError>> {
        Box::pin(async move { Err(FilePortError::from(self.0.clone())) })
    }
    fn get<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: RecordId,
        _: FileGetRequest,
    ) -> BoxFuture<'a, Result<FileDownload, FilePortError>> {
        Box::pin(async move { Err(FilePortError::from(self.0.clone())) })
    }
}

fn app_failing_with(error: ApiError) -> axum::Router {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    let down = Arc::new(Down(error.clone()));
    let services = ApiServices::new(fake, down.clone(), down.clone(), down, store)
        .with_files(Arc::new(DownFiles(error)));
    let site = SiteId::new("acme").must();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build()
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: Json,
    raw: String,
}

async fn send(app: &axum::Router, method: &str, path: &str, body: Option<Json>) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN_ALICE}"));
    let bytes = match body {
        Some(body) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            serde_json::to_vec(&body).must()
        }
        None => Vec::new(),
    };
    let request = builder.body(Body::from(bytes)).must();
    let response = app.clone().oneshot(request).await.must();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .must();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    Reply {
        status,
        headers,
        body: serde_json::from_str(&raw).unwrap_or(Json::Null),
        raw,
    }
}

fn retry_after(reply: &Reply) -> Option<&str> {
    reply
        .headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
}

/// The routes under test: a resource read, a list, a lookup, a file read, a file upload.
async fn probes(app: &axum::Router) -> Vec<(&'static str, Reply)> {
    vec![
        (
            "resource read",
            send(app, "GET", &format!("/api/v1/resource/customer/{ID}"), None).await,
        ),
        (
            "resource list",
            send(app, "GET", "/api/v1/resource/customer", None).await,
        ),
        (
            "lookup",
            send(
                app,
                "POST",
                "/api/v1/lookup/customer/phone",
                Some(json!({"text": "+92"})),
            )
            .await,
        ),
        (
            "file read",
            send(app, "GET", &format!("/api/v1/files/{ID}"), None).await,
        ),
    ]
}

#[tokio::test]
async fn a_storage_unavailable_is_503_with_retry_after_on_every_route() {
    let app = app_failing_with(storage_unavailable_api());
    for (route, reply) in probes(&app).await {
        assert_eq!(reply.status, 503, "{route}: {}", reply.raw);
        assert_eq!(retry_after(&reply), Some("1"), "{route}");
        assert_eq!(reply.body["error"]["code"], "server.unavailable", "{route}");
        assert_eq!(reply.body["error"]["retryable"], true, "{route}");
        // The request id is a random UUIDv7 (`xxxxxxxx-xxdb-7xxx-...` contains the leak marker
        // `db-7` one time in 256), so it is taken out before the marker scan.
        let scanned = match reply.body["error"]["request_id"].as_str() {
            Some(id) => reply.raw.replace(id, ""),
            None => reply.raw.clone(),
        };
        for leak in LEAKS {
            assert!(
                !scanned.contains(leak),
                "{route} leaks `{leak}`: {}",
                reply.raw
            );
        }
    }
}

fn storage_unavailable_api() -> ApiError {
    storage_error_to_api(&storage_unavailable(), None)
}

#[tokio::test]
async fn a_larger_wait_is_rendered_as_is() {
    let app = app_failing_with(storage_error_to_api(&storage_unavailable(), Some(30)));
    for (route, reply) in probes(&app).await {
        // The file port carries the wait too (WP-PLT-01.T7).
        assert_eq!(retry_after(&reply), Some("30"), "{route}");
    }
}

#[tokio::test]
async fn a_service_that_forgot_the_wait_still_sends_one_for_a_503() {
    let app = app_failing_with(ApiError::from_code(Code::ServerUnavailable));
    let reply = send(
        &app,
        "GET",
        &format!("/api/v1/resource/customer/{ID}"),
        None,
    )
    .await;
    assert_eq!(reply.status, 503);
    assert_eq!(retry_after(&reply), Some("1"));
}

#[tokio::test]
async fn a_throttled_login_carries_its_wait() {
    let error = ApiError::from_code(Code::ApiRateLimited).detail("retry_after", json!(42));
    let app = app_failing_with(error);
    let reply = send(
        &app,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": "alice", "password": "x"})),
    )
    .await;
    assert_eq!(reply.status, 429, "{}", reply.raw);
    assert_eq!(retry_after(&reply), Some("42"));
    assert_eq!(reply.body["error"]["details"]["retry_after"], 42);
}

#[tokio::test]
async fn a_locked_consumer_carries_its_wait_and_a_zero_becomes_one() {
    let error = ApiError::from_code(Code::EventstreamConsumerBusy).detail("retry_after", json!(7));
    let app = app_failing_with(error);
    let reply = send(
        &app,
        "GET",
        &format!("/api/v1/resource/customer/{ID}"),
        None,
    )
    .await;
    assert_eq!(reply.status, 409);
    assert_eq!(retry_after(&reply), Some("7"));

    let zero = ApiError::from_code(Code::ApiRateLimited).detail("retry_after", json!(0));
    let app = app_failing_with(zero);
    let reply = send(
        &app,
        "GET",
        &format!("/api/v1/resource/customer/{ID}"),
        None,
    )
    .await;
    assert_eq!(retry_after(&reply), Some("1"));
}

#[tokio::test]
async fn an_error_with_no_wait_carries_no_header() {
    for error in [
        ApiError::from_code(Code::DocumentNotFound),
        ApiError::from_code(Code::ApiRateLimited),
        storage_error_to_api(&StorageError::new(ErrorKind::Internal, "x"), Some(9)),
    ] {
        let app = app_failing_with(error);
        let reply = send(
            &app,
            "GET",
            &format!("/api/v1/resource/customer/{ID}"),
            None,
        )
        .await;
        assert!(retry_after(&reply).is_none(), "{}", reply.raw);
    }
}
