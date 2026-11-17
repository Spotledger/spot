//! The single sign-on administration routes (contract 0.17.0, WP-CON-03.T40; WP-IAM-05.T10):
//! `sso_admin_provider_*` and `sso_admin_policy_*` over [`SsoAdminPort`]. Split from
//! `sso.rs` (the public sign-in group) to keep each file under the 1,000-line gate; the rules
//! stated there apply here too (no secret logged or echoed, every response `no-store`, bounded
//! input, unknown fields refused).
//!
//! The layer passes the port no actor, tenant or role. Before **every write** it asks
//! [`SsoAdminPort::step_up_current`]: no recent step-up is `403 auth.step_up_required` and the
//! operation is not called. An error from the port is a refusal, never an allow, and only the
//! codes of the contract are let through.
//!
//! A registry route file (authenticated: it declares no `PUBLIC`): `SERVES`, `routes()` and
//! `required_ports()`; the public sign-in group is `sso.rs`.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::{Map, Value as Json};

use super::RouteState;
use super::sso::{
    MAX_BODY_BYTES, code_failure, failure, finish, internal, media_type, parse_fields, read_body,
    request_id_of, valid_provider,
};
use crate::api::ports::PortKey;
use crate::api::ports::sso::{AdminPage, AdminReply, SsoAdminPort};
use crate::api::registry::Code;
use crate::api::service::{ApiError, FieldIssue, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::build_ctx;

/// The administration operation ids.
pub const SERVES: &[&str] = &[
    "sso_admin_provider_list",
    "sso_admin_provider_create",
    "sso_admin_provider_get",
    "sso_admin_provider_put",
    "sso_admin_provider_delete",
    "sso_admin_provider_test",
    "sso_admin_policy_get",
    "sso_admin_policy_put",
];

/// The ports the administration group needs: the administration port, which carries the
/// permission check and the audit of every outcome (never half wired).
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn SsoAdminPort>()];
    &KEYS
}

/// The administration routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route(
            "/api/v1/identity/sso/providers",
            get(admin_provider_list).post(admin_provider_create),
        )
        .route(
            "/api/v1/identity/sso/providers/{id}",
            get(admin_provider_get)
                .put(admin_provider_put)
                .delete(admin_provider_delete),
        )
        .route(
            "/api/v1/identity/sso/providers/{id}/test",
            post(admin_provider_test),
        )
        .route(
            "/api/v1/identity/sso/policy",
            get(admin_policy_get).put(admin_policy_put),
        )
}

/// The codes an administration port may answer with; anything else is `http.internal` (and the
/// code is logged by name only), so a port cannot invent a status or leak through a code.
const ADMIN_CODES: &[Code] = &[
    Code::AuthRequired,
    Code::AuthSessionExpired,
    Code::AuthStepUpRequired,
    Code::PermissionDenied,
    Code::SsoProviderNotFound,
    Code::SsoProviderInvalid,
    Code::SsoWouldLockOut,
    Code::ApiInvalidRequest,
    Code::ApiPreconditionRequired,
    Code::ApiIdempotencyConflict,
    Code::ApiRateLimited,
    Code::DocumentStaleVersion,
    Code::ServerUnavailable,
];

fn admin_error(error: ApiError, request_id: &RequestId) -> Response {
    let allowed = matches!(&error.code, crate::api::service::ErrorCode::Registry(c) if ADMIN_CODES.contains(c));
    if allowed {
        failure(error, request_id)
    } else {
        tracing::error!(
            "the single sign-on administration port answered with a code outside the contract"
        );
        code_failure(Code::HttpInternal, request_id)
    }
}

/// The caller's context, or `401 auth.required` when no bearer was sent (the router's layer has
/// already refused that; this keeps the handler safe on its own).
#[allow(clippy::result_large_err)]
fn admin_ctx(
    site: &SiteContext,
    headers: &HeaderMap,
    key: Option<String>,
) -> Result<RequestCtx, Response> {
    let ctx = build_ctx(site, headers, key);
    if ctx.credential.is_none() {
        return Err(code_failure(Code::AuthRequired, &site.request_id));
    }
    Ok(ctx)
}

fn valid_id(id: &str) -> bool {
    valid_provider(id)
}

/// The `Idempotency-Key` of the contract: 1 to 128 visible ASCII characters, exactly one.
fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let mut all = headers.get_all("idempotency-key").iter();
    let value = all.next()?.to_str().ok()?;
    if all.next().is_some() || !(1..=128).contains(&value.len()) {
        return None;
    }
    value
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b))
        .then(|| value.to_owned())
}

/// `If-Match` of the contract: a quoted positive integer, exactly one.
fn if_match(headers: &HeaderMap) -> Option<String> {
    let mut all = headers.get_all(header::IF_MATCH).iter();
    let value = all.next()?.to_str().ok()?;
    if all.next().is_some() {
        return None;
    }
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.is_empty()
        && inner.len() <= 18
        && !inner.starts_with('0')
        && inner.bytes().all(|b| b.is_ascii_digit()))
    .then(|| value.to_owned())
}

fn etag_ok(etag: &str) -> bool {
    etag.len() <= 40
        && etag
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .is_some_and(|inner| {
                !inner.is_empty()
                    && !inner.starts_with('0')
                    && inner.bytes().all(|b| b.is_ascii_digit())
            })
}

fn admin_json(status: StatusCode, reply: &AdminReply, request_id: &RequestId) -> Response {
    let mut body = Map::new();
    body.insert("data".to_owned(), reply.data.clone());
    if let Some(next) = &reply.next_cursor {
        match next {
            Some(cursor) if (1..=2048).contains(&cursor.len()) => {
                body.insert("next_cursor".to_owned(), Json::String(cursor.clone()));
            }
            Some(_) => {
                return internal(
                    request_id,
                    "the administration port returned a cursor outside the contract",
                );
            }
            None => {
                body.insert("next_cursor".to_owned(), Json::Null);
            }
        }
    }
    let mut response = (status, axum::Json(Json::Object(body))).into_response();
    if let Some(etag) = &reply.etag {
        match HeaderValue::from_str(etag) {
            Ok(value) if etag_ok(etag) => {
                response.headers_mut().insert(header::ETAG, value);
            }
            _ => {
                return internal(
                    request_id,
                    "the administration port returned an ETag outside the contract",
                );
            }
        }
    }
    finish(response)
}

/// Asks the port for the step-up marker; `Some(response)` is the refusal to return.
async fn require_step_up(
    port: &dyn SsoAdminPort,
    ctx: &RequestCtx,
    request_id: &RequestId,
) -> Option<Response> {
    match port.step_up_current(ctx).await {
        Ok(true) => None,
        Ok(false) => Some(code_failure(Code::AuthStepUpRequired, request_id)),
        // An error is a refusal, never an allow.
        Err(error) => Some(admin_error(error, request_id)),
    }
}

fn provider_invalid(path: &str) -> ApiError {
    ApiError::new(
        Code::SsoProviderInvalid,
        "The provider definition is not valid.",
    )
    .with_field(FieldIssue::new(
        path,
        "sso.provider_invalid",
        "This value is not valid.",
    ))
}

fn string_within(value: &Json, max: usize) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.chars().count() <= max && !s.chars().any(char::is_control))
}

/// Checks the shape of a provider definition by the contract's `SsoProviderInput` bounds; the
/// first problem is reported as a field path (never a value). The port checks the meaning.
fn check_provider_input(body: &Map<String, Json>) -> Result<(), ApiError> {
    const KEYS: &[&str] = &[
        "kind", "name", "audience", "enabled", "oidc", "saml", "jit", "note",
    ];
    if let Some(key) = body.keys().find(|k| !KEYS.contains(&k.as_str())) {
        // The key name is attacker-chosen: only a fixed path goes back.
        let _ = key;
        return Err(provider_invalid("$"));
    }
    for required in ["kind", "name", "audience"] {
        if !body.contains_key(required) {
            return Err(provider_invalid(required));
        }
    }
    if !matches!(body["kind"].as_str(), Some("oidc" | "saml")) {
        return Err(provider_invalid("kind"));
    }
    match body["name"].as_str() {
        Some(name)
            if (1..=100).contains(&name.chars().count()) && string_within(&body["name"], 100) => {}
        _ => return Err(provider_invalid("name")),
    }
    match body["audience"].as_array() {
        Some(list)
            if list.len() <= 2
                && list
                    .iter()
                    .all(|a| matches!(a.as_str(), Some("staff" | "partner"))) => {}
        _ => return Err(provider_invalid("audience")),
    }
    if body.get("enabled").is_some_and(|v| !v.is_boolean()) {
        return Err(provider_invalid("enabled"));
    }
    if body
        .get("jit")
        .is_some_and(|v| !matches!(v.as_str(), Some("off" | "match_only" | "create")))
    {
        return Err(provider_invalid("jit"));
    }
    if body.get("note").is_some_and(|v| !string_within(v, 500)) {
        return Err(provider_invalid("note"));
    }
    if let Some(oidc) = body.get("oidc") {
        let Some(oidc) = oidc.as_object() else {
            return Err(provider_invalid("oidc"));
        };
        for (key, value) in oidc {
            let ok = match key.as_str() {
                "issuer" => string_within(value, 2048),
                "client_id" => string_within(value, 512),
                "client_secret_name" => string_within(value, 200),
                "scopes" => value
                    .as_array()
                    .is_some_and(|l| l.len() <= 20 && l.iter().all(|s| string_within(s, 100))),
                "claims_map" => value.as_object().is_some_and(|m| {
                    m.len() <= 20
                        && m.iter()
                            .all(|(k, v)| k.len() <= 200 && string_within(v, 200))
                }),
                _ => false,
            };
            if !ok {
                return Err(provider_invalid(&format!(
                    "oidc.{}",
                    if key.len() <= 20 && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                        key.as_str()
                    } else {
                        "?"
                    }
                )));
            }
        }
        for required in ["issuer", "client_id"] {
            if !oidc.contains_key(required) {
                return Err(provider_invalid(&format!("oidc.{required}")));
            }
        }
    }
    if let Some(saml) = body.get("saml") {
        let Some(saml) = saml.as_object() else {
            return Err(provider_invalid("saml"));
        };
        for (key, value) in saml {
            let ok = match key.as_str() {
                "metadata_url" => string_within(value, 2048),
                "entity_id" => string_within(value, 1024),
                _ => false,
            };
            if !ok {
                return Err(provider_invalid("saml"));
            }
        }
    }
    Ok(())
}

fn check_policy_input(body: &Map<String, Json>) -> Result<(), ApiError> {
    let bad = |field: &str| {
        ApiError::new(Code::ApiInvalidRequest, "The sign-in policy is not valid.").with_field(
            FieldIssue::new(field, "api.invalid_request", "This value is not valid."),
        )
    };
    if body.keys().any(|k| k != "mode" && k != "break_glass_users") {
        return Err(bad("$"));
    }
    if !matches!(
        body.get("mode").and_then(Json::as_str),
        Some("local" | "sso_optional" | "sso_required")
    ) {
        return Err(bad("mode"));
    }
    match body.get("break_glass_users").and_then(Json::as_array) {
        Some(list)
            if list.len() <= 20
                && list.iter().all(|u| {
                    u.as_str().is_some_and(|s| {
                        (1..=64).contains(&s.len()) && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
                    })
                }) => {}
        _ => return Err(bad("break_glass_users")),
    }
    Ok(())
}

/// A JSON object body (`Content-Type: application/json`, at most 64 KiB).
async fn read_object(request: Request) -> Result<Map<String, Json>, ApiError> {
    let (headers, bytes) = read_body(request, MAX_BODY_BYTES).await?;
    if media_type(&headers).as_deref() != Some("application/json") {
        return Err(ApiError::from_code(Code::ApiUnsupportedMediaType));
    }
    match serde_json::from_slice::<Json>(&bytes) {
        Ok(Json::Object(map)) => Ok(map),
        Ok(_) => Err(ApiError::new(
            Code::ApiInvalidRequest,
            "The body must be a JSON object.",
        )),
        Err(_) => Err(ApiError::from_code(Code::ApiInvalidJson)),
    }
}

fn page_of(query: &str) -> Result<AdminPage, ApiError> {
    let invalid = |m: &str| ApiError::new(Code::ApiInvalidRequest, m.to_owned());
    let fields = parse_fields(query, &[("cursor", 2048), ("limit", 3)])
        .ok_or_else(|| invalid("`cursor` and `limit` are the only query parameters."))?;
    let cursor = match fields.get("cursor") {
        Some(c) if c.is_empty() => return Err(invalid("`cursor` must not be empty.")),
        Some(c) => Some(c.clone()),
        None => None,
    };
    let limit = match fields.get("limit") {
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=100).contains(n) && v.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| invalid("`limit` must be a whole number from 1 to 100."))?,
        None => 50,
    };
    Ok(AdminPage { cursor, limit })
}

/// The administration port and the caller's context, or the response to return.
#[allow(clippy::result_large_err)]
fn admin_start(
    state: &RouteState,
    site: &SiteContext,
    headers: &HeaderMap,
    key: Option<String>,
) -> Result<(std::sync::Arc<dyn SsoAdminPort>, RequestCtx), Response> {
    let Some(port) = state.port::<dyn SsoAdminPort>() else {
        return Err(code_failure(Code::HttpInternal, &site.request_id));
    };
    let ctx = admin_ctx(site, headers, key)?;
    Ok((port, ctx))
}

async fn admin_provider_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let (port, ctx) = match admin_start(&state, &site, &headers, None) {
        Ok(started) => started,
        Err(response) => return response,
    };
    let page = match page_of(request.uri().query().unwrap_or("")) {
        Ok(page) => page,
        Err(error) => return failure(error, request_id),
    };
    match port.list_providers(&ctx, &page).await {
        Ok(reply) if reply.next_cursor.is_some() => admin_json(StatusCode::OK, &reply, request_id),
        Ok(_) => internal(
            request_id,
            "the administration port returned a list without a cursor member",
        ),
        Err(error) => admin_error(error, request_id),
    }
}

async fn admin_provider_create(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(key) = idempotency_key(&headers) else {
        return failure(
            ApiError::new(
                Code::ApiInvalidRequest,
                "An Idempotency-Key header is required.",
            ),
            request_id,
        );
    };
    let (port, ctx) = match admin_start(&state, &site, &headers, Some(key.clone())) {
        Ok(started) => started,
        Err(response) => return response,
    };
    let body = match read_object(request).await {
        Ok(body) => body,
        Err(error) => return failure(error, request_id),
    };
    if let Err(error) = check_provider_input(&body) {
        return failure(error, request_id);
    }
    if let Some(refusal) = require_step_up(port.as_ref(), &ctx, request_id).await {
        return refusal;
    }
    match port.create_provider(&ctx, &key, &body).await {
        Ok(reply) => admin_json(StatusCode::CREATED, &reply, request_id),
        Err(error) => admin_error(error, request_id),
    }
}

async fn admin_provider_get(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = request_id_of(&site);
    let (port, ctx) = match admin_start(&state, &site, &headers, None) {
        Ok(started) => started,
        Err(response) => return response,
    };
    if !valid_id(&id) {
        return code_failure(Code::SsoProviderNotFound, request_id);
    }
    match port.get_provider(&ctx, &id).await {
        Ok(reply) => admin_json(StatusCode::OK, &reply, request_id),
        Err(error) => admin_error(error, request_id),
    }
}

async fn admin_provider_put(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let (port, ctx) = match admin_start(&state, &site, &headers, None) {
        Ok(started) => started,
        Err(response) => return response,
    };
    if !valid_id(&id) {
        return code_failure(Code::SsoProviderNotFound, request_id);
    }
    let Some(version) = if_match(&headers) else {
        return code_failure(Code::ApiPreconditionRequired, request_id);
    };
    let body = match read_object(request).await {
        Ok(body) => body,
        Err(error) => return failure(error, request_id),
    };
    if let Err(error) = check_provider_input(&body) {
        return failure(error, request_id);
    }
    if let Some(refusal) = require_step_up(port.as_ref(), &ctx, request_id).await {
        return refusal;
    }
    match port.put_provider(&ctx, &id, &version, &body).await {
        Ok(reply) => admin_json(StatusCode::OK, &reply, request_id),
        Err(error) => admin_error(error, request_id),
    }
}

async fn admin_provider_delete(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = request_id_of(&site);
    let (port, ctx) = match admin_start(&state, &site, &headers, None) {
        Ok(started) => started,
        Err(response) => return response,
    };
    if !valid_id(&id) {
        return code_failure(Code::SsoProviderNotFound, request_id);
    }
    let Some(version) = if_match(&headers) else {
        return code_failure(Code::ApiPreconditionRequired, request_id);
    };
    if let Some(refusal) = require_step_up(port.as_ref(), &ctx, request_id).await {
        return refusal;
    }
    match port.delete_provider(&ctx, &id, &version).await {
        Ok(()) => finish(StatusCode::NO_CONTENT.into_response()),
        Err(error) => admin_error(error, request_id),
    }
}

async fn admin_provider_test(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = request_id_of(&site);
    let (port, ctx) = match admin_start(&state, &site, &headers, None) {
        Ok(started) => started,
        Err(response) => return response,
    };
    if !valid_id(&id) {
        return code_failure(Code::SsoProviderNotFound, request_id);
    }
    if let Some(refusal) = require_step_up(port.as_ref(), &ctx, request_id).await {
        return refusal;
    }
    match port.test_provider(&ctx, &id).await {
        Ok(reply) => admin_json(StatusCode::OK, &reply, request_id),
        Err(error) => admin_error(error, request_id),
    }
}

async fn admin_policy_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
) -> Response {
    let request_id = request_id_of(&site);
    let (port, ctx) = match admin_start(&state, &site, &headers, None) {
        Ok(started) => started,
        Err(response) => return response,
    };
    match port.get_policy(&ctx).await {
        Ok(reply) => admin_json(StatusCode::OK, &reply, request_id),
        Err(error) => admin_error(error, request_id),
    }
}

async fn admin_policy_put(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let (port, ctx) = match admin_start(&state, &site, &headers, None) {
        Ok(started) => started,
        Err(response) => return response,
    };
    let Some(version) = if_match(&headers) else {
        return code_failure(Code::ApiPreconditionRequired, request_id);
    };
    let body = match read_object(request).await {
        Ok(body) => body,
        Err(error) => return failure(error, request_id),
    };
    if let Err(error) = check_policy_input(&body) {
        return failure(error, request_id);
    }
    if let Some(refusal) = require_step_up(port.as_ref(), &ctx, request_id).await {
        return refusal;
    }
    match port.put_policy(&ctx, &version, &body).await {
        Ok(reply) => admin_json(StatusCode::OK, &reply, request_id),
        Err(error) => admin_error(error, request_id),
    }
}
