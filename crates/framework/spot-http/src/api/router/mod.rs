//! The `/api/v1` router (ADR-055): every contract operation except `realtime_connect`
//! (WP-PLT-08.T2), mounted through `ServerBuilder::site_routes`.
//!
//! Layout: [`middleware`] carries the bearer and `Idempotency-Key` middlewares, [`auth`]
//! the login/logout/session handlers, [`resource`] the meta and document endpoints, and
//! [`lifecycle`] status actions, method/function calls and reports. This module holds the
//! wire assembly, the `Failure` rendering type and the helpers the handlers share
//! (site context, shared body decoding, response envelopes).
//!
//! Error envelope: `[{"_": {"message", "code"}}]`; success envelope: `[{"d": data},
//! {"messages": optional}]` (ADR-055). Site context is `SiteContext`; the request id
//! `RequestId`.
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::Value as Json;
use spot_storage_contract::{Name, RecordId, Version};

use crate::request::RequestId;
use crate::site::SiteContext;

use super::codec::{ApiDocument, MetaLookup, encode_document, parse_record_id};
use super::files::FilesPort;
use super::idempotency::IdempotencyStore;
use super::meta::DocTypeMeta;
use super::ports::Ports;
use super::print::PrintPort;
use super::registry::Code;
use super::service::{
    ApiError, AuthService, Credential, FieldIssue, LookupService, Message, MetaService, Page,
    PermissionService, RequestCtx, ResourceService, RevealService, WriteResult,
};
use middleware::header_credential;

/// Largest body this layer reads (defensive; the server core enforces the configured limit
/// before routing and maps chunked overruns itself).
const MAX_READ_BODY: usize = 10 * 1024 * 1024;

/// The header stamped on replayed idempotent responses.
pub const REPLAYED_HEADER: &str = "idempotency-replayed";

/// The services bundle `api::router` consumes.
pub struct ApiServices {
    /// DocType metadata.
    pub meta: Arc<dyn MetaService>,
    /// Documents, lifecycle, methods, functions, reports.
    pub resource: Arc<dyn ResourceService>,
    /// Field lookups (ADR-068 §3.5).
    pub lookup: Arc<dyn LookupService>,
    /// Login/logout/session.
    pub auth: Arc<dyn AuthService>,
    /// The `Idempotency-Key` store (24 h TTL, scoped by site and credential).
    pub idempotency: Arc<dyn IdempotencyStore>,
    /// The permission summary (`auth_permissions`); the route is mounted only when set.
    pub permissions: Option<Arc<dyn PermissionService>>,
    /// Field reveal (`field_reveal`); the route is mounted only when set.
    pub reveal: Option<Arc<dyn RevealService>>,
    /// The typed ports of the registered route groups (`routes::REGISTRY`), plus the file store
    /// behind `file_upload`/`file_get` and the print service behind `print_get`. The file and
    /// print routes are always mounted; with no store or service they answer `501
    /// service.not_configured` so a site without files or printing still starts. A registered
    /// group is mounted only when every port it requires is here.
    pub ports: Ports,
    /// The clock the public groups' rate limits are timed by (monotonic intervals only).
    /// `ApiServices::new` starts with the system clock; a host with one clock passes it with
    /// [`ApiServices::with_clock`], tests pass a manual one.
    pub clock: Arc<dyn spot_core::Clock>,
}

impl ApiServices {
    /// Assemble the service bundle.
    pub fn new(
        meta: Arc<dyn MetaService>,
        resource: Arc<dyn ResourceService>,
        lookup: Arc<dyn LookupService>,
        auth: Arc<dyn AuthService>,
        idempotency: Arc<dyn IdempotencyStore>,
    ) -> Self {
        ApiServices {
            meta,
            resource,
            lookup,
            auth,
            idempotency,
            permissions: None,
            reveal: None,
            ports: Ports::new(),
            clock: Arc::new(spot_core::SystemClock::new()),
        }
    }

    /// Times the public groups' rate limits by `clock`.
    pub fn with_clock(mut self, clock: Arc<dyn spot_core::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Provides a port a registered route group requires (its trait is in `api::ports`).
    pub fn with_port<T: ?Sized + Send + Sync + 'static>(mut self, port: Arc<T>) -> Self {
        self.ports.provide(port);
        self
    }

    /// Serves `auth_permissions` from this service.
    pub fn with_permissions(mut self, permissions: Arc<dyn PermissionService>) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// Serves `field_reveal` from this service.
    pub fn with_reveal(mut self, reveal: Arc<dyn RevealService>) -> Self {
        self.reveal = Some(reveal);
        self
    }

    /// Serves `file_upload` and `file_get` from this store.
    pub fn with_files(mut self, files: Arc<dyn FilesPort>) -> Self {
        self.ports.provide::<dyn FilesPort>(files);
        self
    }

    /// Serves `print_get` from this service.
    pub fn with_print(mut self, print: Arc<dyn PrintPort>) -> Self {
        self.ports.provide::<dyn PrintPort>(print);
        self
    }
}

#[derive(Clone)]
struct SharedState {
    meta: Arc<dyn MetaService>,
    resource: Arc<dyn ResourceService>,
    lookup: Arc<dyn LookupService>,
    auth: Arc<dyn AuthService>,
    idempotency: Arc<dyn IdempotencyStore>,
    permissions: Option<Arc<dyn PermissionService>>,
    reveal: Option<Arc<dyn RevealService>>,
    files: Option<Arc<dyn FilesPort>>,
    print: Option<Arc<dyn PrintPort>>,
}

/// A validated `Idempotency-Key` the middleware already checked.
#[derive(Clone, Debug)]
struct ValidatedKey(String);

/// The contract operations the hand-wired routes below serve (realtime excluded). Registered
/// groups (`routes::REGISTRY`) add to it; a registered group may not serve an id listed here.
/// The routes were not moved into groups (see `docs/running-spot/http-routes.md`), so
/// this list is the only hand-kept part: the conformance test checks every id against the
/// contract and derives the gap list from it and `routes::registry_serves()`.
pub const HAND_WIRED_SERVES: &[&str] = &[
    "auth_login",
    "auth_logout",
    "auth_session",
    "auth_permissions",
    "meta_get",
    "lookup_search",
    "resource_list",
    "resource_create",
    "resource_query",
    "resource_get",
    "resource_update",
    "resource_delete",
    "field_reveal",
    "lifecycle_post",
    "lifecycle_cancel",
    "lifecycle_correct",
    "lifecycle_rename",
    "document_method_call",
    "function_call",
    "function_call_get",
    "report_run",
    "file_upload",
    "file_get",
    "print_get",
];

/// Every operation id a router built by [`router`] serves when all its optional services are
/// wired: the hand-wired ones plus the registered groups'.
pub fn served_operations() -> std::collections::BTreeSet<&'static str> {
    let mut served: std::collections::BTreeSet<&'static str> =
        HAND_WIRED_SERVES.iter().copied().collect();
    served.extend(routes::registry_serves());
    served
}

/// Build the `/api/v1` router (every contract operation except realtime).
pub fn router(services: ApiServices) -> Router {
    router_with(services, routes::REGISTRY).0
}

/// [`router`] over an explicit list of route groups (tests register their own), and what was
/// mounted. Every group sits behind the bearer check, like the hand-wired secured routes,
/// except a group that declares `PUBLIC` (`routes::Public`): it is merged outside the bearer
/// layer, behind the rate limit of its declared class instead.
pub fn router_with(
    services: ApiServices,
    registrations: &[routes::Registration],
) -> (Router, routes::MountReport) {
    let mounted = routes::mount(registrations, &services.ports, HAND_WIRED_SERVES);
    let report = mounted.report;
    let registered = mounted
        .authenticated
        .layer(axum::middleware::from_fn(middleware::auth_middleware));
    // One limiter for the router: one counter per rate class and client address.
    let limiter = Arc::new(middleware::RateLimiter::new(services.clock.clone()));
    let mut public = Router::new();
    for (class, group) in mounted.public {
        public = public.merge(group.layer(axum::middleware::from_fn_with_state(
            middleware::RateGuard {
                limiter: limiter.clone(),
                class,
            },
            middleware::public_middleware,
        )));
    }
    let state = SharedState {
        meta: services.meta,
        resource: services.resource,
        lookup: services.lookup,
        auth: services.auth,
        idempotency: services.idempotency,
        permissions: services.permissions,
        reveal: services.reveal,
        files: services.ports.get::<dyn FilesPort>(),
        print: services.ports.get::<dyn PrintPort>(),
    };
    // Commands that declare `Idempotency-Key` in the contract.
    let commands = Router::new()
        .route(
            "/api/v1/resource/{doctype}",
            post(resource::resource_create),
        )
        .route(
            "/api/v1/resource/{doctype}/{id}/post",
            post(lifecycle::lifecycle_post),
        )
        .route(
            "/api/v1/resource/{doctype}/{id}/cancel",
            post(lifecycle::lifecycle_cancel),
        )
        .route(
            "/api/v1/resource/{doctype}/{id}/correct",
            post(lifecycle::lifecycle_correct),
        )
        .route(
            "/api/v1/resource/{doctype}/{id}/rename",
            post(lifecycle::lifecycle_rename),
        )
        .route(
            "/api/v1/resource/{doctype}/{id}/methods/{method}",
            post(lifecycle::document_method_call),
        )
        .route(
            "/api/v1/method/{app}/{module}/{function}",
            post(lifecycle::function_call_post),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::idempotency_middleware,
        ));
    let mut secured = Router::new()
        .route("/api/v1/auth/logout", post(auth::auth_logout))
        .route("/api/v1/auth/session", get(auth::auth_session))
        .route("/api/v1/meta/{doctype}", get(resource::meta_get))
        .route("/api/v1/resource/{doctype}", get(resource::resource_list))
        .route(
            "/api/v1/resource/{doctype}/query",
            post(resource::resource_query),
        )
        .route(
            "/api/v1/resource/{doctype}/{id}",
            get(resource::resource_get)
                .patch(resource::resource_update)
                .delete(resource::resource_delete),
        )
        .route(
            "/api/v1/method/{app}/{module}/{function}",
            get(lifecycle::function_call_get),
        )
        .route("/api/v1/report/{report}/run", post(lifecycle::report_run))
        .route(
            "/api/v1/lookup/{doctype}/{field}",
            post(lookup::lookup_search),
        )
        // Files: not behind the JSON idempotency middleware (it buffers); the upload keeps
        // the `Idempotency-Key` protocol itself.
        .route("/api/v1/files", post(files::file_upload))
        .route("/api/v1/files/{id}", get(files::file_get))
        // A read-only GET: no body, no Idempotency-Key; the service decides permission.
        .route("/api/v1/print/{doctype}/{id}", get(print::print_get))
        .merge(commands);
    // The 0.5.0 routes exist only when their service is wired. Reveal is not a command: the
    // idempotency layer never sees it, so `Idempotency-Key` is ignored.
    if state.permissions.is_some() {
        secured = secured.route(
            "/api/v1/auth/permissions",
            get(permissions::auth_permissions),
        );
    }
    if state.reveal.is_some() {
        secured = secured.route(
            "/api/v1/resource/{doctype}/{id}/reveal",
            post(permissions::field_reveal),
        );
    }
    let secured = secured
        .with_state(state.clone())
        .layer(axum::middleware::from_fn(middleware::auth_middleware));
    let unauthenticated = Router::new().route(
        "/api/v1/auth/login",
        post(auth::auth_login).with_state(state),
    );
    (
        secured
            .merge(unauthenticated)
            .merge(registered)
            .merge(public),
        report,
    )
}

/// Login, logout and session handlers.
mod auth;
/// File upload and download (`/api/v1/files`).
mod files;
/// Status actions (`post`/`cancel`/`correct`/`rename`), method and function calls, reports.
mod lifecycle;
/// The field-lookup endpoint (`POST /api/v1/lookup/{doctype}/{field}`, ADR-068 §3.5).
mod lookup;
/// Bearer enforcement and the `Idempotency-Key` command middleware.
mod middleware;
/// The permission summary and field reveal (contract 0.5.0).
mod permissions;
/// Print a document to a PDF (`GET /api/v1/print/{doctype}/{id}`, contract 0.7.0).
mod print;
/// Meta and document endpoints (meta get, list/query/get/create/patch/delete).
mod resource;
/// Served operation groups that register themselves (one file each).
pub mod routes;
// ---------------------------------------------------------------------------
// Errors

/// A rendered HTTP failure: an `ApiError` plus the request id it is reported under. The
/// error is boxed to keep `Result` returns small (clippy `result_large_err`).
struct Failure {
    error: Box<ApiError>,
    request_id: RequestId,
}

fn fail(error: ApiError, request_id: &RequestId) -> Failure {
    Failure {
        error: Box::new(error),
        request_id: request_id.clone(),
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        render_error(*self.error, &self.request_id)
    }
}

type HandlerResult = Result<Response, Failure>;

/// Render an `ApiError` with the registry's status and `retryable` into the error envelope.
/// An unregistered app code at the top level is a service bug: it is logged, reported as
/// `http.internal` with the app code preserved under `details.app_code`.
fn render_error(error: ApiError, request_id: &RequestId) -> Response {
    let mut error = error;
    let status = match error.code {
        super::service::ErrorCode::Registry(_) => error.status(),
        super::service::ErrorCode::App(ref app_code) => {
            tracing::error!(code = %app_code, "unregistered top-level error code");
            error
                .details
                .insert("app_code".to_owned(), Json::String(app_code.clone()));
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    // A 503 `server.unavailable` always tells the client how long to wait: a service (or a
    // port that cannot carry details) that sent none gets the pool-wait default.
    if error.code == super::service::ErrorCode::Registry(Code::ServerUnavailable)
        && super::storage_error::retry_after_secs(&error).is_none()
    {
        error.details.insert(
            super::storage_error::RETRY_AFTER_DETAIL.to_owned(),
            Json::from(super::storage_error::POOL_WAIT_RETRY_AFTER_SECS),
        );
    }
    let wait = super::storage_error::retry_after_secs(&error);
    let body = error.to_envelope_json(request_id.as_str());
    let mut response = (status, axum::Json(body)).into_response();
    if let Some(seconds) = wait {
        // Whole seconds, at least 1 (a `Retry-After: 0` invites a hot retry loop).
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
    }
    response
}

// ---------------------------------------------------------------------------
// Shared request helpers

fn language_of(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())?;
    let first = raw.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

/// SHA-256 of the bearer token, hex; the credential half of an idempotency scope. Keys are
/// scoped by site and credential so two callers never see each other's entries (ADR-055).
fn credential_hash(credential: Option<&Credential>) -> String {
    match credential {
        None => "anon".to_owned(),
        Some(c) => {
            use sha2::Digest as _;
            let digest = sha2::Sha256::digest(c.token.as_bytes());
            let mut out = String::with_capacity(digest.len() * 2);
            for b in digest {
                use std::fmt::Write as _;
                let _ = write!(&mut out, "{b:02x}");
            }
            out
        }
    }
}

/// `If-Match: "<n>"` (the contract's strong ETag pattern `"[1-9][0-9]*"`).
fn parse_if_match(
    headers: &HeaderMap,
    required: bool,
    request_id: &RequestId,
) -> Result<Option<Version>, Failure> {
    let Some(raw) = headers.get("if-match") else {
        if required {
            return Err(fail(
                ApiError::from_code(Code::ApiPreconditionRequired),
                request_id,
            ));
        }
        return Ok(None);
    };
    let raw = raw.to_str().map_err(|_| {
        fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "If-Match must be a quoted decimal version.",
            ),
            request_id,
        )
    })?;
    let inner = raw
        .trim()
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'));
    let valid = inner.is_some_and(|inner| {
        !inner.is_empty()
            && !inner.starts_with('0')
            && inner.len() <= 20
            && inner.bytes().all(|b| b.is_ascii_digit())
    });
    if !valid {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                format!("`{raw}` is not a strong ETag like \"3\"."),
            ),
            request_id,
        ));
    }
    let parsed = inner
        .and_then(|inner| inner.parse::<u64>().ok())
        .filter(|n| *n >= 1);
    match parsed {
        Some(n) => Ok(Some(Version(n))),
        None => Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "`If-Match` version is out of range.",
            ),
            request_id,
        )),
    }
}

fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    let Some(raw) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    raw.split(',').any(|candidate| {
        let candidate = candidate.trim();
        let strong = candidate.strip_prefix("W/").unwrap_or(candidate);
        strong == etag || strong == "*"
    })
}

fn build_ctx(site: &SiteContext, headers: &HeaderMap, key: Option<String>) -> RequestCtx {
    RequestCtx {
        site: site.site.clone(),
        credential: header_credential(headers),
        idempotency_key: key,
        language: language_of(headers),
        request_id: site.request_id.clone(),
        peer: site.peer,
    }
}

fn validated_key(request: &Request) -> Option<String> {
    request
        .extensions()
        .get::<ValidatedKey>()
        .map(|k| k.0.clone())
}

/// The JSON body for a handler: the idempotency middleware re-attaches the bytes it read for
/// the fingerprint, so handlers read the body the same way with or without a key. Content
/// type (`415`), size (`413`) and JSON syntax (`400 api.invalid_json`) are enforced here; the
/// middleware enforces syntax before reserving a key so a bad request never holds the key.
/// `required` routes reject an absent body.
async fn take_body(
    request: Request,
    required: bool,
    request_id: &RequestId,
) -> Result<Option<Json>, Failure> {
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    if let Some(ct) = &content_type
        && ct != "application/json"
    {
        return Err(fail(
            ApiError::from_code(Code::ApiUnsupportedMediaType),
            request_id,
        ));
    }
    let bytes = axum::body::to_bytes(body, MAX_READ_BODY)
        .await
        .map_err(|_| fail(ApiError::from_code(Code::HttpBodyTooLarge), request_id))?;
    if bytes.is_empty() {
        if required {
            return Err(fail(
                ApiError::new(
                    Code::ApiInvalidJson,
                    "The request body must be a JSON document.",
                ),
                request_id,
            ));
        }
        return Ok(None);
    }
    let parsed: Json = serde_json::from_slice(&bytes)
        .map_err(|_| fail(ApiError::from_code(Code::ApiInvalidJson), request_id))?;
    Ok(Some(parsed))
}

fn path_identifier(raw: &str, what: &str, request_id: &RequestId) -> Result<(), Failure> {
    if Name::new(raw).is_ok() {
        Ok(())
    } else {
        Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                format!("`{raw}` is not a valid {what} storage name."),
            ),
            request_id,
        ))
    }
}

fn path_record_id(raw: &str, request_id: &RequestId) -> Result<RecordId, Failure> {
    parse_record_id(raw).ok_or_else(|| {
        fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "A document id must be a canonical lower-case UUIDv7.",
            ),
            request_id,
        )
    })
}

/// Resolve the parent DocType's meta plus one level of child metas, for the codec and the
/// filter validator (both synchronous).
async fn prepare_meta(
    state: &SharedState,
    ctx: &RequestCtx,
    doctype: &str,
) -> Result<(DocTypeMeta, BTreeMap<String, DocTypeMeta>), ApiError> {
    let parent = state.meta.doctype_meta(ctx, doctype).await?;
    let mut map = BTreeMap::new();
    map.insert(parent.doctype.as_str().to_owned(), parent.clone());
    for field in &parent.fields {
        if field.field_type == super::meta::FieldType::Table
            && let Some(child) = &field.child_doctype
            && !map.contains_key(child.as_str())
        {
            let child_meta = state.meta.doctype_meta(ctx, child.as_str()).await?;
            map.insert(child.as_str().to_owned(), child_meta);
        }
    }
    Ok((parent, map))
}

fn lookup_over(map: &BTreeMap<String, DocTypeMeta>) -> MetaLookup<'_> {
    MetaLookup(map)
}

// ---------------------------------------------------------------------------
// Response builders

fn json_response(status: StatusCode, body: Json) -> Response {
    (status, axum::Json(body)).into_response()
}

/// The success envelope `{"data": ..., "messages": [...]?}`; `messages` is omitted when
/// empty (the contract's "Responses" section).
fn envelope(data: Json, messages: &[Message]) -> Json {
    let mut map = serde_json::Map::new();
    map.insert("data".to_owned(), data);
    if !messages.is_empty() {
        map.insert(
            "messages".to_owned(),
            Json::Array(messages.iter().map(message_json).collect()),
        );
    }
    Json::Object(map)
}

fn message_json(m: &Message) -> Json {
    let mut map = serde_json::Map::new();
    map.insert(
        "level".to_owned(),
        Json::String(m.level.as_str().to_owned()),
    );
    map.insert("code".to_owned(), Json::String(m.code.clone()));
    map.insert("message".to_owned(), Json::String(m.text.clone()));
    if let Some(field) = &m.field {
        map.insert("field".to_owned(), Json::String(field.clone()));
    }
    Json::Object(map)
}

fn stamp_etag(response: &mut Response, document: &ApiDocument) {
    if let Some(version) = document.version
        && let Ok(value) = HeaderValue::from_str(&format!("\"{}\"", version.0))
    {
        response.headers_mut().insert(header::ETAG, value);
    }
}

fn stamp_location(response: &mut Response, doctype: &str, id: RecordId) {
    if let Ok(value) = HeaderValue::from_str(&format!("/api/v1/resource/{doctype}/{id}")) {
        response.headers_mut().insert(header::LOCATION, value);
    }
}

/// A one-document response with ETag (and `Location` on creates).
fn document_response(
    status: StatusCode,
    result: &WriteResult,
    meta: &DocTypeMeta,
    metas: &BTreeMap<String, DocTypeMeta>,
    location_for: Option<&str>,
    request_id: &RequestId,
) -> Result<Response, Failure> {
    let data = encode_document(&result.document, meta, None, lookup_over(metas))
        .map_err(|e| fail(e, request_id))?;
    let mut response = json_response(status, envelope(data, &result.messages));
    stamp_etag(&mut response, &result.document);
    if let Some(doctype) = location_for
        && let Some(id) = result.document.id
    {
        stamp_location(&mut response, doctype, id);
    }
    Ok(response)
}

fn page_response(
    page: &Page,
    meta: &DocTypeMeta,
    metas: &BTreeMap<String, DocTypeMeta>,
    request_id: &RequestId,
) -> Result<Response, Failure> {
    let mut data = Vec::with_capacity(page.documents.len());
    for doc in &page.documents {
        data.push(
            encode_document(doc, meta, None, lookup_over(metas))
                .map_err(|e| fail(e, request_id))?,
        );
    }
    let mut map = serde_json::Map::new();
    map.insert("data".to_owned(), Json::Array(data));
    map.insert(
        "next_cursor".to_owned(),
        match &page.next_cursor {
            Some(c) => Json::String(c.clone()),
            None => Json::Null,
        },
    );
    if let Some(total) = page.total {
        map.insert("total".to_owned(), Json::from(total));
    }
    Ok(json_response(StatusCode::OK, Json::Object(map)))
}

fn unknown_field_error(meta: &DocTypeMeta, field: &str) -> ApiError {
    let mut err = ApiError::new(
        Code::DocumentUnknownField,
        format!("The DocType `{}` has no such field.", meta.doctype.as_str()),
    );
    err.fields.push(FieldIssue::new(
        field.to_owned(),
        "document.unknown_field",
        "The DocType has no such field.",
    ));
    err
}

fn checked_name(part: &str, request_id: &RequestId) -> Result<Name, Failure> {
    Name::new(part).map_err(|_| {
        fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                format!("`{part}` is not a field name."),
            ),
            request_id,
        )
    })
}

fn parse_limit(n: Option<i64>, request_id: &RequestId) -> Result<u32, Failure> {
    match n {
        None => Ok(20),
        Some(n) => {
            if !(1..=500).contains(&n) {
                return Err(fail(
                    ApiError::new(
                        Code::ApiInvalidRequest,
                        "`limit` must be between 1 and 500.",
                    ),
                    request_id,
                ));
            }
            Ok(n as u32)
        }
    }
}

// ---------------------------------------------------------------------------
// Auth routes
