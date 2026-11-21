//! WP-PLT-05.T12: `GET /meta/{doctype}` names a secret `secret` and a multiselect
//! `table_multiselect` as the contract does, and a field the caller may not write is refused in a
//! create body (not silently dropped, nothing stored), never echoed in the error or in an
//! idempotency replay.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod form_support;

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use form_support::{meta as invoice_meta, now, permission_engine, principals, resolver};
use serde_json::{Value as Json, json};
use spot_api_services::{EngineResources, MetaResources};
use spot_core::{ManualClock, SiteId};
use spot_document::engine::{Engine, PermissionAuthorizer, V7Ids};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::router::{ApiServices, router};
use spot_http::api::service::*;
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;
use tower::ServiceExt;

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

/// A router over `meta` with the permission authorizer and the form-support principals.
async fn app(meta: Arc<EffectiveMeta>, enforce: bool) -> Router {
    let clock = Arc::new(ManualClock::new(now()));
    let mut engine = Engine::new(
        FakeBackend::new(),
        meta.clone(),
        Arc::new(RegistryBuilder::new().build().unwrap()),
        clock.clone(),
        Arc::new(V7Ids),
    )
    .unwrap();
    if enforce {
        engine = engine.with_authorizer(Arc::new(PermissionAuthorizer::new(
            permission_engine(),
            Arc::new(principals()),
        )));
    }
    let engine = Arc::new(engine);
    engine.prepare().await.unwrap();
    let resources = Arc::new(EngineResources::new(
        engine,
        meta.clone(),
        clock.clone(),
        resolver(),
    ));
    let services = ApiServices::new(
        Arc::new(MetaResources::new(meta)),
        resources,
        Arc::new(Unused),
        Arc::new(Unused),
        Arc::new(MemoryIdempotencyStore::new(clock)),
    );
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new().add("test.example", SiteId::new("test").unwrap()),
    )
    .site_routes(router(services))
    .build()
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Json>,
) -> (u16, String, Json) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "test.example")
        .header("authorization", "Bearer form-user");
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_slice(&bytes).unwrap_or(Json::Null);
    (status, raw, json)
}

/// A DocType with a secret and a table_multiselect (the child has exactly one link).
fn vault_meta() -> Arc<EffectiveMeta> {
    let mut tag = Definition::new("tag");
    tag.fields = vec![FieldSpec::new("label", FieldType::Text)];
    let mut row = Definition::new("tag_row");
    row.is_child = true;
    let mut link = FieldSpec::new("tag", FieldType::Link);
    link.options = Some("tag".to_owned());
    row.fields = vec![link];
    let mut vault = Definition::new("vault");
    let pin = FieldSpec::new("pin", FieldType::Secret);
    let mut tags = FieldSpec::new("tags", FieldType::TableMultiselect);
    tags.options = Some("tag_row".to_owned());
    vault.fields = vec![FieldSpec::new("title", FieldType::Text), pin, tags];
    Registry::new(vec![tag, row, vault]).unwrap().effective()
}

#[tokio::test]
async fn meta_names_a_secret_and_a_multiselect_as_the_contract_does() {
    let app = app(vault_meta(), false).await;
    let (status, raw, body) = call(&app, "GET", "/api/v1/meta/vault", &[], None).await;
    assert_eq!(status, 200, "{raw}");
    let fields = body["data"]["fields"].as_array().unwrap();
    let field = |name: &str| fields.iter().find(|f| f["name"] == name).unwrap();
    assert_eq!(field("pin")["type"], "secret", "{raw}");
    assert!(
        field("pin").get("default").is_none(),
        "no secret default: {raw}"
    );
    assert_eq!(field("tags")["type"], "table_multiselect", "{raw}");
    assert_eq!(field("tags")["child_doctype"], "tag_row", "{raw}");
    assert_eq!(field("tags")["link_field"], "tag", "{raw}");
    assert_eq!(field("title")["type"], "text");
}

const MARK: &str = "TOPSECRET-WRITE-MARK";

#[tokio::test]
async fn a_field_the_caller_may_not_write_is_refused_and_never_echoed() {
    let app = app(invoice_meta(), true).await;
    // `hidden_note` is hidden from the signed-in user; `secret` is read_masked only.
    for field in ["hidden_note", "secret"] {
        let body = json!({"number": "INV-1", field: MARK});
        let (status, raw, json) = call(
            &app,
            "POST",
            "/api/v1/resource/invoice",
            &[("idempotency-key", &format!("key-{field}"))],
            Some(body.clone()),
        )
        .await;
        assert!(status >= 400, "{field}: accepted: {raw}");
        // A write refusal is `permission.denied` naming each field with the field's own code.
        assert_eq!(
            json["error"]["code"], "permission.denied",
            "{field}: the refusal is the contract's code, not a silent drop: {raw}"
        );
        assert_eq!(
            json["error"]["fields"][0]["code"], "permission.field_denied",
            "{field}: the field is named with its own code: {raw}"
        );
        assert!(!raw.contains(MARK), "{field}: the value is echoed: {raw}");

        // The same key replays the same answer, still without the value.
        let (again_status, again_raw, _) = call(
            &app,
            "POST",
            "/api/v1/resource/invoice",
            &[("idempotency-key", &format!("key-{field}"))],
            Some(body),
        )
        .await;
        assert!(again_status >= 400, "{again_raw}");
        assert!(
            !again_raw.contains(MARK),
            "{field}: replay echoes: {again_raw}"
        );
    }
    // Nothing was stored by any refused call.
    let (status, raw, json) = call(
        &app,
        "POST",
        "/api/v1/resource/invoice/query",
        &[],
        Some(json!({"limit": 10})),
    )
    .await;
    assert_eq!(status, 200, "{raw}");
    assert_eq!(json["data"].as_array().map(Vec::len), Some(0), "{raw}");
    assert!(!raw.contains(MARK));
}
