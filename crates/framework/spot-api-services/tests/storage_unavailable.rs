//! WP-PLT-05.T6: a storage failure that reaches the native API through the real adapters keeps
//! its `Retry-After` and never becomes `http.internal` when it is really unavailability.
//!
//! The backend is the fake one behind a wrapper that answers every transaction and read with a
//! chosen `StorageError`; the requests go through the real `spot-http` router and the real
//! `EngineResources`, `MetaResources` and `Engine`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value as Json, json};
use spot_api_services::{EngineResources, MetaResources};
use spot_app_api::context::Actor;
use spot_core::{ManualClock, SiteId};
use spot_document::engine::{Engine, V7Ids};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::router::{ApiServices, router};
use spot_http::api::service::*;
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_meta::{Definition, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{
    Capabilities, Catalog, ChangeFeed, ErrorKind, FeedPosition, HealthReport, Query, ReadSource,
    Rows, StorageBackend, StorageError, StorageResult, TxOptions,
};
use spot_storage_fake::FakeBackend;
use tower::ServiceExt;

/// The fake backend, failing every transaction and read with `fail` when it is set.
struct Flaky {
    inner: FakeBackend,
    fail: Arc<Mutex<Option<ErrorKind>>>,
}

impl Flaky {
    fn error(&self) -> Option<StorageError> {
        // The explanation is what a driver would say; it must never reach a client.
        let kind = (*self.fail.lock().unwrap())?;
        Some(StorageError::new(
            kind,
            "pool pg-host-7.internal:5432 exhausted (password=hunter2)",
        ))
    }
}

impl ChangeFeed for Flaky {
    async fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<spot_storage_contract::OutboxRecord>> {
        self.inner.read_outbox(after, limit).await
    }

    async fn read_changes(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<spot_storage_contract::Change>> {
        self.inner.read_changes(after, limit).await
    }
}

impl StorageBackend for Flaky {
    type Tx = <FakeBackend as StorageBackend>::Tx;

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn prepare(&self, catalog: &Catalog) -> StorageResult<()> {
        self.inner.prepare(catalog).await
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<Self::Tx> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        self.inner.begin(catalog, options).await
    }

    async fn read(
        &self,
        catalog: Arc<Catalog>,
        source: ReadSource,
        query: &Query,
    ) -> StorageResult<Rows> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        self.inner.read(catalog, source, query).await
    }

    async fn health(&self) -> HealthReport {
        self.inner.health().await
    }
}

struct Unused;
impl AuthService for Unused {
    fn login<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>> {
        Box::pin(async { panic!("auth out of scope") })
    }
    fn logout<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async { panic!("auth out of scope") })
    }
    fn session<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>> {
        Box::pin(async { panic!("auth out of scope") })
    }
    fn audience_of<'a>(
        &'a self,
        _: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<spot_core::Audience, ApiError>> {
        Box::pin(async { Ok(spot_core::Audience::Staff) })
    }
}
impl LookupService for Unused {
    fn search<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
        _: &'a LookupRequest,
    ) -> BoxFuture<'a, Result<LookupPage, ApiError>> {
        Box::pin(async { panic!("lookup out of scope") })
    }
}

struct Rig {
    router: Router,
    fail: Arc<Mutex<Option<ErrorKind>>>,
}

async fn rig() -> Rig {
    let defs = vec![
        Definition::parse("note.toml", include_str!("fixtures/note.toml")).unwrap(),
        Definition::parse("note_row.toml", include_str!("fixtures/note_row.toml")).unwrap(),
    ];
    let meta = Registry::new(defs).unwrap().effective();
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
    ));
    let fail = Arc::new(Mutex::new(None));
    let engine = Arc::new(
        Engine::new(
            Flaky {
                inner: FakeBackend::new(),
                fail: fail.clone(),
            },
            meta.clone(),
            Arc::new(RegistryBuilder::new().build().unwrap()),
            clock.clone(),
            Arc::new(V7Ids),
        )
        .unwrap(),
    );
    engine.prepare().await.unwrap();
    let user =
        spot_http::api::codec::parse_record_id("01970000-0000-7000-8000-000000000001").unwrap();
    let resources = Arc::new(EngineResources::new(
        engine,
        meta.clone(),
        clock.clone(),
        Arc::new(move |_| {
            Ok(Actor::User {
                id: user,
                via: None,
            })
        }),
    ));
    let services = ApiServices::new(
        Arc::new(MetaResources::new(meta)),
        resources,
        Arc::new(Unused),
        Arc::new(Unused),
        Arc::new(MemoryIdempotencyStore::new(clock)),
    );
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new().add("test.example", SiteId::new("test").unwrap()),
    )
    .site_routes(router(services))
    .build();
    Rig { router, fail }
}

struct Reply {
    status: u16,
    retry_after: Option<String>,
    raw: String,
    body: Json,
}

async fn call(rig: &Rig, method: &str, path: &str, body: Option<Json>) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "test.example")
        .header("authorization", "Bearer any");
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = rig
        .router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = serde_json::from_slice(&bytes).unwrap_or(Json::Null);
    Reply {
        status,
        retry_after,
        raw,
        body,
    }
}

const ID: &str = "01970000-0000-7000-8000-0000000000aa";

/// The routes that reach storage through the adapters: a document read, a list query, a create
/// and an update. (`meta` and `auth_permissions` read the in-memory metadata and never reach
/// storage; the form event is covered by the unit test of its error stripping.)
fn routes() -> Vec<(&'static str, String, Option<Json>)> {
    vec![
        ("GET", format!("/api/v1/resource/note/{ID}"), None),
        (
            "POST",
            "/api/v1/resource/note/query".to_owned(),
            Some(json!({"limit": 5})),
        ),
        (
            "POST",
            "/api/v1/resource/note".to_owned(),
            Some(json!({"title": "x"})),
        ),
    ]
}

#[tokio::test]
async fn unavailability_answers_503_with_a_wait_and_no_connection_detail() {
    for kind in [ErrorKind::Unavailable, ErrorKind::LockTimeout] {
        let rig = rig().await;
        *rig.fail.lock().unwrap() = Some(kind);
        for (method, path, body) in routes() {
            let reply = call(&rig, method, &path, body).await;
            assert_eq!(reply.status, 503, "{kind:?} {method} {path}: {}", reply.raw);
            assert_eq!(reply.body["error"]["code"], "server.unavailable", "{path}");
            assert!(
                reply.retry_after.is_some(),
                "{kind:?} {path}: no Retry-After"
            );
            assert_eq!(
                reply.body["error"]["details"]["retry_after"],
                json!(1),
                "{kind:?} {path}: {}",
                reply.raw
            );
            for leak in ["pg-host-7", "5432", "hunter2", "exhausted"] {
                assert!(
                    !reply.raw.contains(leak),
                    "{path} leaks `{leak}`: {}",
                    reply.raw
                );
            }
        }
    }
}

#[tokio::test]
async fn not_found_stays_404_and_conflict_exhaustion_keeps_its_code() {
    let rig = rig().await;
    let reply = call(&rig, "GET", &format!("/api/v1/resource/note/{ID}"), None).await;
    assert_eq!(reply.status, 404, "{}", reply.raw);
    assert!(reply.retry_after.is_none());

    *rig.fail.lock().unwrap() = Some(ErrorKind::SerializationFailure);
    let reply = call(&rig, "GET", &format!("/api/v1/resource/note/{ID}"), None).await;
    assert_eq!(reply.status, 503, "{}", reply.raw);
    assert_eq!(
        reply.body["error"]["code"], "server.conflict_retry_exhausted",
        "{}",
        reply.raw
    );
    assert_eq!(reply.body["error"]["details"]["retry_after"], json!(1));
}
