#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value as Json, json};
use spot_api_services::{Delegates, EngineResources, MetaResources};
use spot_app_api::context::Actor;
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::{
    AppResult, BoxFuture as AppFuture, Controller, HookContext, HookPoint, Phase, Target,
};
use spot_core::{ManualClock, SiteId};
use spot_document::engine::{Engine, V7Ids};
use spot_events_contract::lifecycle::Operation;
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::registry::Code;
use spot_http::api::{
    idempotency::MemoryIdempotencyStore,
    router::{ApiServices, router},
    service::*,
};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_meta::{Definition, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{Name, RecordId};
use spot_storage_fake::FakeBackend;
use std::sync::Arc;
use tower::ServiceExt;

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}
pub fn user() -> RecordId {
    spot_http::api::codec::parse_record_id("01970000-0000-7000-8000-000000000001").unwrap()
}
pub fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

struct Messages;
impl Controller for Messages {
    fn doctype(&self) -> &str {
        "note"
    }
    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("probe:message").unwrap(),
            point: HookPoint::of(Phase::Before, Target::Op(Operation::Create)),
            priority: 0,
            budget: std::time::Duration::from_secs(5),
        }]
    }
    fn run_step<'a>(
        &'a self,
        _: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> AppFuture<'a, AppResult<()>> {
        Box::pin(async move {
            assert_eq!(ctx.invocation().site.as_str(), "test");
            assert_eq!(
                ctx.invocation().actor,
                Actor::User {
                    id: user(),
                    via: None
                }
            );
            assert_eq!(ctx.invocation().language.as_deref(), Some("ur-PK"));
            assert_eq!(ctx.invocation().now, now());
            ctx.message(spot_app_api::Message::info(
                "document.created",
                "Created by real lifecycle",
            ));
            Ok(())
        })
    }
}

// Deliberately unimplemented test-only collaborators, never used for resource requests.
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

pub struct App {
    pub router: Router,
    pub engine: Arc<Engine<FakeBackend>>,
    pub resources: Arc<EngineResources<Engine<FakeBackend>>>,
}
impl App {
    pub async fn new() -> Self {
        Self::with_delegates(Delegates::none()).await
    }
    pub async fn with_delegates(delegates: Delegates) -> Self {
        let defs = vec![
            Definition::parse("note.toml", include_str!("../fixtures/note.toml")).unwrap(),
            Definition::parse("note_row.toml", include_str!("../fixtures/note_row.toml")).unwrap(),
        ];
        let meta = Registry::new(defs).unwrap().effective();
        let clock = Arc::new(ManualClock::new(now()));
        let mut registry = RegistryBuilder::new();
        registry.add_controller("probe", Arc::new(Messages));
        let engine = Arc::new(
            Engine::new(
                FakeBackend::new(),
                meta.clone(),
                Arc::new(registry.build().unwrap()),
                clock.clone(),
                Arc::new(V7Ids),
            )
            .unwrap(),
        );
        engine.prepare().await.unwrap();
        let resources = Arc::new(
            EngineResources::new(
                engine.clone(),
                meta.clone(),
                clock.clone(),
                Arc::new(|ctx| {
                    if ctx.site.as_str() != "test"
                        || ctx.credential.as_ref().map(|c| c.token.as_str())
                            != Some("resolved-test-user")
                    {
                        return Err(ApiError::from_code(Code::AuthRequired));
                    }
                    Ok(Actor::User {
                        id: user(),
                        via: None,
                    })
                }),
            )
            .with_delegates(delegates),
        );
        let services = ApiServices::new(
            Arc::new(MetaResources::new(meta)),
            resources.clone(),
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
        Self {
            router,
            engine,
            resources,
        }
    }
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Option<Json>,
    ) -> Call {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "test.example")
            .header("authorization", "Bearer resolved-test-user")
            .header("accept-language", "ur-PK");
        for (key, value) in headers {
            request = request.header(*key, value);
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let body = if bytes.is_empty() {
            Json::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        Call {
            status,
            headers,
            body,
        }
    }
    pub async fn create(&self, title: &str) -> Call {
        let r = self
            .call(
                "POST",
                "/api/v1/resource/note",
                &[],
                Some(json!({"title":title})),
            )
            .await;
        r.status(201);
        r
    }
}
pub struct Call {
    pub status: u16,
    pub headers: axum::http::HeaderMap,
    pub body: Json,
}
impl Call {
    pub fn status(&self, n: u16) {
        assert_eq!(self.status, n, "{}", self.body);
    }
    pub fn id(&self) -> &str {
        self.body["data"]["id"].as_str().unwrap()
    }
    pub fn etag(&self) -> String {
        self.headers["etag"].to_str().unwrap().to_owned()
    }
    pub fn path(&self) -> String {
        format!("/api/v1/resource/note/{}", self.id())
    }
}
