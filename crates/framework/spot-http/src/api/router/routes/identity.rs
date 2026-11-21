//! The `/api/v1/identity/` administration group (contract 0.18.0, WP-CON-03.T41,
//! WP-IAM-12.T1): users, invitations, roles, assignments, assignment rules, API keys, SCIM
//! credentials, administrative session control and support impersonation, behind the ports of
//! `ports/identity.rs`.
//!
//! # What the layer does, in this order, for every operation
//!
//! 1. A bearer is present (else `401 auth.required`).
//! 2. The area the operation needs is filled in [`IdentityAdminPort`] (else `501
//!    service.not_configured`).
//! 3. The permission port names the caller and says the platform permission is held. **Fail
//!    closed:** any error, and any answer other than a clean "held", is a refusal; a consumer is
//!    refused everywhere and a partner on every read.
//! 4. A write needs a recent step-up (`403 auth.step_up_required`) except the dry-run preview and
//!    ending an impersonation. The check runs before the body is read and before the port.
//! 5. The query, the path id, the body (at most 256 KiB, a closed object: an unknown field is
//!    refused, so no body can name an actor, an approver or a tenant) and the headers are bounded
//!    and shape-checked; `Idempotency-Key` and `If-Match` are required where the contract says.
//! 6. The port is called; its error is rendered as it is.
//!
//! Every refusal and failure of steps 1 to 6 is audited through the audit port (operation, code,
//! actor and target id, never a body, a secret or an address). A success is audited by the adapter
//! that made the change. Every answer is `Cache-Control: private, no-store`.
//!
//! The acting user is taken from the credential only. This group sits outside the
//! `Idempotency-Key` middleware, so nothing here is ever stored for replay; the secret-bearing
//! answers (a new API key, a new SCIM credential) are therefore never in a replay cache, and the
//! port receives the key as `ctx.idempotency_key` to dedupe the command itself.
//!
//! **Registered** in the route registry (`SERVES`, `routes`, `required_ports`): `build.rs` mounts it
//! when all four ports are present, and `tests/conformance.rs` walks its 33 operations.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Map, Value as Json, json};

use super::RouteState;
use crate::api::ports::PortKey;
use crate::api::ports::identity::{
    ApiKeyCreateIn, AssignmentCreateIn, AssignmentListIn, AuditEntry, AuditOutcome, CallerAudience,
    GrantOut, IdentityAdminPort, IdentityAuditPort, IdentityCaller, IdentityPermission,
    IdentityPermissionPort, IdentityStepUpPort, ImpersonationIn, PageIn, RecoverOut, RuleIn,
    SessionListIn, UserCreateIn, UserListIn, UserPatchIn,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, BoxFuture, ErrorCode, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{build_ctx, render_error};

/// The operation ids this group serves once registered.
#[rustfmt::skip]
pub const SERVES: &[&str] = &[
    "identity_user_list", "identity_user_create", "identity_user_get", "identity_user_patch",
    "identity_user_disable", "identity_user_enable", "identity_user_anonymise",
    "identity_user_recover", "identity_invitation_list", "identity_invitation_revoke",
    "identity_invitation_resend", "identity_role_list", "identity_role_get",
    "identity_role_customise", "identity_role_upgrade_diff", "identity_assignment_list",
    "identity_assignment_create", "identity_assignment_delete", "identity_assignment_rules_get",
    "identity_assignment_rules_put", "identity_assignment_rules_preview",
    "identity_assignment_rules_run", "identity_api_key_list", "identity_api_key_create",
    "identity_api_key_delete", "identity_scim_credential_list",
    "identity_scim_credential_create", "identity_scim_credential_delete",
    "identity_session_list", "identity_session_end", "identity_user_sessions_end_all",
    "identity_impersonation_start", "identity_impersonation_end",
];

/// The ports this group needs: all four, so the group is never half wired.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 4] = [
        PortKey::of::<dyn IdentityAdminPort>(),
        PortKey::of::<dyn IdentityPermissionPort>(),
        PortKey::of::<dyn IdentityStepUpPort>(),
        PortKey::of::<dyn IdentityAuditPort>(),
    ];
    &KEYS
}

/// The identity routes.
pub fn routes() -> Router<RouteState> {
    let p = "/api/v1/identity";
    Router::new()
        .route(&format!("{p}/users"), get(user_list).post(user_create))
        .route(
            &format!("{p}/users/{{id}}"),
            get(user_get).patch(user_patch),
        )
        .route(&format!("{p}/users/{{id}}/disable"), post(user_disable))
        .route(&format!("{p}/users/{{id}}/enable"), post(user_enable))
        .route(&format!("{p}/users/{{id}}/anonymise"), post(user_anonymise))
        .route(&format!("{p}/users/{{id}}/recover"), post(user_recover))
        .route(
            &format!("{p}/users/{{id}}/sessions/end-all"),
            post(sessions_end_all),
        )
        .route(&format!("{p}/invitations"), get(invitation_list))
        .route(
            &format!("{p}/invitations/{{id}}"),
            delete(invitation_revoke),
        )
        .route(
            &format!("{p}/invitations/{{id}}/resend"),
            post(invitation_resend),
        )
        .route(&format!("{p}/roles"), get(role_list))
        .route(&format!("{p}/roles/{{id}}"), get(role_get))
        .route(&format!("{p}/roles/{{id}}/customise"), post(role_customise))
        .route(&format!("{p}/roles/{{id}}/upgrade-diff"), get(role_diff))
        .route(
            &format!("{p}/assignments"),
            get(assignment_list).post(assignment_create),
        )
        .route(
            &format!("{p}/assignments/{{id}}"),
            delete(assignment_delete),
        )
        .route(
            &format!("{p}/assignment-rules"),
            get(rules_get).put(rules_put),
        )
        .route(
            &format!("{p}/assignment-rules/preview"),
            post(rules_preview),
        )
        .route(&format!("{p}/assignment-rules/run"), post(rules_run))
        .route(
            &format!("{p}/api-keys"),
            get(api_key_list).post(api_key_create),
        )
        .route(&format!("{p}/api-keys/{{id}}"), delete(api_key_delete))
        .route(
            &format!("{p}/scim-credentials"),
            get(scim_list).post(scim_create),
        )
        .route(&format!("{p}/scim-credentials/{{id}}"), delete(scim_delete))
        .route(&format!("{p}/sessions"), get(session_list))
        .route(&format!("{p}/sessions/{{id}}"), delete(session_end))
        .route(
            &format!("{p}/impersonation"),
            post(impersonation_start).delete(impersonation_end),
        )
}

// ---------------------------------------------------------------------------
// The shape of one operation

/// The area of [`IdentityAdminPort`] an operation needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Area {
    Users,
    Roles,
    Assignments,
    Keys,
    Sessions,
}

/// What the layer enforces for one operation.
#[derive(Clone, Copy)]
struct Spec {
    op: &'static str,
    area: Area,
    perm: IdentityPermission,
    /// A write: partner audience allowed (reads are staff only).
    write: bool,
    step_up: bool,
    idem: bool,
    if_match: bool,
    body: bool,
    id: bool,
    query: &'static [&'static str],
}

#[rustfmt::skip]
impl Spec {
    const fn rd(op: &'static str, area: Area) -> Spec {
        Spec { op, area, perm: IdentityPermission::Read, write: false, step_up: false, idem: false, if_match: false, body: false, id: false, query: &[] }
    }
    const fn wr(op: &'static str, area: Area, perm: IdentityPermission) -> Spec {
        Spec { write: true, step_up: true, perm, ..Spec::rd(op, area) }
    }
    const fn idem(self) -> Spec { Spec { idem: true, ..self } }
    const fn ifm(self) -> Spec { Spec { if_match: true, ..self } }
    const fn body(self) -> Spec { Spec { body: true, ..self } }
    const fn id(self) -> Spec { Spec { id: true, ..self } }
    const fn no_step(self) -> Spec { Spec { step_up: false, ..self } }
    const fn q(self, query: &'static [&'static str]) -> Spec { Spec { query, ..self } }
}

/// The validated request an operation's closure gets.
struct Req {
    ctx: RequestCtx,
    id: String,
    q: BTreeMap<String, String>,
    body: Json,
    version: u64,
}

/// What an operation answers.
struct Reply {
    status: StatusCode,
    body: Option<Json>,
    etag: Option<u64>,
}

#[rustfmt::skip]
impl Reply {
    fn with(status: u16, body: Option<Json>) -> Reply {
        Reply { status: StatusCode::from_u16(status).unwrap_or(StatusCode::OK), body, etag: None }
    }
    fn data(status: u16, data: Json) -> Reply { Reply::with(status, Some(json!({ "data": data }))) }
    fn raw(body: Json) -> Reply { Reply::with(200, Some(body)) }
    fn status(status: u16) -> Reply { Reply::with(status, None) }
    fn etag(self, version: u64) -> Reply { Reply { etag: Some(version), ..self } }
}

// ---------------------------------------------------------------------------
// The runner

fn finish(result: Result<Reply, ApiError>, request_id: &RequestId) -> Response {
    let mut response = match result {
        Ok(reply) => {
            let mut r = match reply.body {
                Some(body) => (reply.status, axum::Json(body)).into_response(),
                None => reply.status.into_response(),
            };
            if let Some(version) = reply.etag
                && let Ok(value) = HeaderValue::from_str(&format!("\"{version}\""))
            {
                r.headers_mut().insert(header::ETAG, value);
            }
            r
        }
        Err(error) => render_error(error, request_id),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has not configured this part of identity administration.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = (StatusCode::NOT_IMPLEMENTED, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

/// Whether the error is the layer's own shape refusal (audited as `Refused`).
fn is_shape_refusal(error: &ApiError) -> bool {
    matches!(
        error.code,
        ErrorCode::Registry(
            Code::ApiInvalidRequest
                | Code::ApiInvalidJson
                | Code::ApiUnsupportedMediaType
                | Code::HttpBodyTooLarge
                | Code::ApiPreconditionRequired
                | Code::AuthStepUpRequired
                | Code::PermissionDenied
        )
    )
}

/// The permission refusal: a port error is kept only when it is one of the sign-in or
/// availability answers; anything else is the plain denial.
fn permission_refusal(error: ApiError) -> ApiError {
    match error.code {
        ErrorCode::Registry(
            Code::AuthRequired
            | Code::AuthSessionExpired
            | Code::AuthInvalidCredentials
            | Code::AuthAccountDisabled
            | Code::PermissionDenied
            | Code::ServerUnavailable,
        ) => error,
        _ => ApiError::from_code(Code::PermissionDenied),
    }
}

struct Trace {
    actor: Option<String>,
    target: Option<String>,
}

type Closure = Box<
    dyn FnOnce(Arc<dyn IdentityAdminPort>, Req) -> BoxFuture<'static, Result<Reply, ApiError>>
        + Send,
>;

#[allow(clippy::too_many_arguments)]
#[allow(clippy::result_large_err)]
async fn guarded(
    state: &RouteState,
    spec: Spec,
    ctx: &mut RequestCtx,
    headers: &HeaderMap,
    path_id: Option<String>,
    query: &str,
    request: Request,
    trace: &mut Trace,
    f: Closure,
) -> Result<Reply, (ApiError, AuditOutcome)> {
    let refuse = |e: ApiError| {
        let outcome = AuditOutcome::Refused;
        (e, outcome)
    };
    let (Some(admin), Some(perm), Some(step)) = (
        state.port::<dyn IdentityAdminPort>(),
        state.port::<dyn IdentityPermissionPort>(),
        state.port::<dyn IdentityStepUpPort>(),
    ) else {
        return Err((
            ApiError::from_code(Code::HttpInternal),
            AuditOutcome::Failed,
        ));
    };
    // The permission check, fail closed.
    let caller: IdentityCaller = match perm.check(ctx, spec.perm).await {
        Ok(c) => c,
        Err(e) => return Err(refuse(permission_refusal(e))),
    };
    trace.actor = Some(caller.user_id.clone());
    let audience_ok = match caller.audience {
        CallerAudience::Staff => true,
        CallerAudience::Partner => spec.write,
        CallerAudience::Consumer => false,
    };
    if !audience_ok {
        return Err(refuse(ApiError::from_code(Code::PermissionDenied)));
    }
    // The step-up, before the body is read and before the port.
    if spec.step_up && !matches!(step.is_recent(ctx).await, Ok(true)) {
        return Err(refuse(ApiError::from_code(Code::AuthStepUpRequired)));
    }
    let q = parse_query(query, spec.query).map_err(refuse)?;
    let id = match (spec.id, path_id) {
        (true, Some(raw)) => {
            let id = record_id(&raw, "id").map_err(refuse)?;
            trace.target = Some(id.clone());
            id
        }
        _ => String::new(),
    };
    let version = if spec.if_match {
        parse_if_match(headers).map_err(refuse)?
    } else {
        0
    };
    if spec.idem {
        let key = idempotency_key(headers).map_err(refuse)?;
        ctx.idempotency_key = Some(key);
    }
    let body = if spec.body {
        read_body(request).await.map_err(refuse)?
    } else {
        Json::Null
    };
    let req = Req {
        ctx: ctx.clone(),
        id,
        q,
        body,
        version,
    };
    f(admin, req).await.map_err(|e| {
        let outcome = if is_shape_refusal(&e) {
            AuditOutcome::Refused
        } else {
            AuditOutcome::Failed
        };
        (e, outcome)
    })
}

async fn run(
    state: RouteState,
    site: SiteContext,
    headers: HeaderMap,
    path_id: Option<String>,
    request: Request,
    spec: Spec,
    f: Closure,
) -> Response {
    let request_id = site.request_id.clone();
    let mut ctx = build_ctx(&site, &headers, None);
    let query = request.uri().query().unwrap_or("").to_owned();
    let (Some(admin), Some(audit)) = (
        state.port::<dyn IdentityAdminPort>(),
        state.port::<dyn IdentityAuditPort>(),
    ) else {
        return finish(Err(ApiError::from_code(Code::HttpInternal)), &request_id);
    };
    let mut trace = Trace {
        actor: None,
        target: None,
    };
    let result = if ctx.credential.is_none() {
        Err((
            ApiError::from_code(Code::AuthRequired),
            AuditOutcome::Refused,
        ))
    } else {
        let present = match spec.area {
            Area::Users => admin.users().is_some(),
            Area::Roles => admin.roles().is_some(),
            Area::Assignments => admin.assignments().is_some(),
            Area::Keys => admin.keys().is_some(),
            Area::Sessions => admin.sessions().is_some(),
        };
        if !present {
            return not_configured(&request_id);
        }
        guarded(
            &state, spec, &mut ctx, &headers, path_id, &query, request, &mut trace, f,
        )
        .await
    };
    match result {
        Ok(reply) => finish(Ok(reply), &request_id),
        Err((error, outcome)) => {
            let entry = AuditEntry {
                operation: spec.op,
                outcome,
                code: error.code.as_str().to_owned(),
                actor: trace.actor,
                target: trace.target,
            };
            if audit.record(&ctx, entry).await.is_err() {
                tracing::error!(operation = spec.op, "identity audit record not written");
            }
            finish(Err(error), &request_id)
        }
    }
}

// ---------------------------------------------------------------------------
// Bounded parsing

fn bad(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                out.push(u8::from_str_radix(raw.get(i + 1..i + 3)?, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The query: only the allowed names, each at most once, values at most 2048 bytes.
fn parse_query(query: &str, allowed: &[&str]) -> Result<BTreeMap<String, String>, ApiError> {
    let mut map = BTreeMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if !allowed.contains(&key) {
            return Err(bad(
                "The request has a query parameter this operation does not take.",
            ));
        }
        let value = percent_decode(value)
            .filter(|v| v.len() <= 2048 && !v.chars().any(char::is_control))
            .ok_or_else(|| bad("A query parameter is not valid text."))?;
        if map.insert(key.to_owned(), value).is_some() {
            return Err(bad("A query parameter can be given only once."));
        }
    }
    Ok(map)
}

fn page_of(q: &BTreeMap<String, String>) -> Result<PageIn, ApiError> {
    let limit = match q.get("limit") {
        None => 50,
        Some(v) => (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
            .then(|| v.parse::<usize>().ok())
            .flatten()
            .filter(|n| (1..=100).contains(n))
            .ok_or_else(|| bad("`limit` must be a whole number from 1 to 100."))?,
    };
    let cursor = match q.get("cursor") {
        None => None,
        Some(c) => Some(
            (!c.is_empty() && c.len() <= 2048 && c.bytes().all(|b| (0x21..=0x7e).contains(&b)))
                .then(|| c.clone())
                .ok_or_else(|| bad("`cursor` must be the `next_cursor` of a previous page."))?,
        ),
    };
    Ok(PageIn { cursor, limit })
}

/// A canonical lower-case UUIDv7 (the contract's `RecordId`).
fn record_id(raw: &str, what: &str) -> Result<String, ApiError> {
    crate::api::codec::parse_record_id(raw)
        .map(|_| raw.to_owned())
        .ok_or_else(|| bad(&format!("`{what}` must be a record id.")))
}

fn opt_record_id(q: &BTreeMap<String, String>, key: &str) -> Result<Option<String>, ApiError> {
    q.get(key).map(|v| record_id(v, key)).transpose()
}

fn parse_if_match(headers: &HeaderMap) -> Result<u64, ApiError> {
    let raw = headers
        .get("if-match")
        .ok_or_else(|| ApiError::from_code(Code::ApiPreconditionRequired))?
        .to_str()
        .map_err(|_| bad("`If-Match` must be a quoted decimal version."))?;
    raw.trim()
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .filter(|s| !s.is_empty() && !s.starts_with('0') && s.len() <= 18)
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| bad("`If-Match` must be a strong ETag like \"3\"."))
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| bad("This operation needs an `Idempotency-Key` header."))?;
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(bad(
            "The `Idempotency-Key` header must be 1 to 128 visible ASCII characters.",
        ));
    }
    Ok(key.to_owned())
}

/// The largest JSON body of the group.
const MAX_BODY: usize = 256 * 1024;

async fn read_body(request: Request) -> Result<Json, ApiError> {
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    if content_type.is_some_and(|ct| ct != "application/json") {
        return Err(ApiError::from_code(Code::ApiUnsupportedMediaType));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| ApiError::from_code(Code::HttpBodyTooLarge))?;
    if bytes.is_empty() {
        return Err(ApiError::new(
            Code::ApiInvalidJson,
            "The request body must be a JSON document.",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::from_code(Code::ApiInvalidJson))
}

/// A closed object: anything but the named fields is refused, so no body can name an actor, an
/// approver or a tenant. The refusal names no field.
fn closed<'a>(body: &'a Json, allowed: &[&str]) -> Result<&'a Map<String, Json>, ApiError> {
    let map = body
        .as_object()
        .ok_or_else(|| bad("The body must be a JSON object."))?;
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(bad("The body has a field this operation does not accept."));
    }
    Ok(map)
}

fn clean(s: &str, min: usize, max: usize) -> bool {
    let n = s.chars().count();
    (min..=max).contains(&n) && !s.chars().any(char::is_control)
}

fn text(m: &Map<String, Json>, key: &str, min: usize, max: usize) -> Result<String, ApiError> {
    m.get(key)
        .and_then(Json::as_str)
        .filter(|s| clean(s, min, max))
        .map(str::to_owned)
        .ok_or_else(|| {
            bad(&format!(
                "`{key}` is missing or not a text of {min} to {max} characters."
            ))
        })
}

fn opt_text(m: &Map<String, Json>, key: &str, max: usize) -> Result<Option<String>, ApiError> {
    match m.get(key) {
        None => Ok(None),
        Some(Json::Null) => Ok(None),
        Some(_) => text(m, key, 0, max).map(Some),
    }
}

fn one_of(m: &Map<String, Json>, key: &str, allowed: &[&str]) -> Result<String, ApiError> {
    let v = text(m, key, 1, 40)?;
    allowed
        .contains(&v.as_str())
        .then_some(v)
        .ok_or_else(|| bad(&format!("`{key}` is not one of the allowed values.")))
}

fn record_field(m: &Map<String, Json>, key: &str) -> Result<String, ApiError> {
    record_id(&text(m, key, 1, 64)?, key)
}

fn instant(m: &Map<String, Json>, key: &str) -> Result<Option<String>, ApiError> {
    let Some(v) = opt_text(m, key, 40)? else {
        return Ok(None);
    };
    time::OffsetDateTime::parse(&v, &time::format_description::well_known::Rfc3339)
        .map(|_| Some(v))
        .map_err(|_| bad(&format!("`{key}` must be an RFC 3339 instant.")))
}

fn text_list(
    m: &Map<String, Json>,
    key: &str,
    max_items: usize,
    max: usize,
) -> Result<Vec<String>, ApiError> {
    let Some(v) = m.get(key) else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .filter(|a| a.len() <= max_items)
        .ok_or_else(|| {
            bad(&format!(
                "`{key}` must be a list of at most {max_items} items."
            ))
        })?;
    arr.iter()
        .map(|i| {
            i.as_str()
                .filter(|s| clean(s, 1, max))
                .map(str::to_owned)
                .ok_or_else(|| bad(&format!("An item of `{key}` is not a valid text.")))
        })
        .collect()
}

fn rules_of(v: &Json) -> Result<Vec<RuleIn>, ApiError> {
    let m = closed(v, &["rules", "version"])?;
    if let Some(version) = m.get("version")
        && !version.as_u64().is_some_and(|n| n >= 1)
    {
        return Err(bad("`version` must be a whole number from 1."));
    }
    let list = m
        .get("rules")
        .and_then(Json::as_array)
        .filter(|a| a.len() <= 200)
        .ok_or_else(|| bad("`rules` must be a list of at most 200 rules."))?;
    list.iter()
        .map(|r| {
            let o = closed(r, &["id", "when", "role_id", "scope", "enabled"])?;
            Ok(RuleIn {
                id: text(o, "id", 1, 64)?,
                when: text(o, "when", 1, 1000)?,
                role_id: record_field(o, "role_id")?,
                scope: opt_text(o, "scope", 200)?,
                enabled: o
                    .get("enabled")
                    .and_then(Json::as_bool)
                    .ok_or_else(|| bad("`enabled` must be true or false."))?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The handlers

/// Defines one handler: its spec, the area accessor and the closure body.
macro_rules! op {
    ($name:ident, $spec:expr, |$a:ident = $area:ident, $r:ident| $body:expr) => {
        async fn $name(
            State(state): State<RouteState>,
            site: SiteContext,
            headers: HeaderMap,
            request: Request,
        ) -> Response {
            let f: Closure = Box::new(
                |port: Arc<dyn IdentityAdminPort>,
                 $r: Req|
                 -> BoxFuture<'static, Result<Reply, ApiError>> {
                    Box::pin(async move {
                        let out: Result<Reply, ApiError> = async {
                            let $a = port
                                .$area()
                                .ok_or_else(|| ApiError::from_code(Code::HttpInternal))?;
                            $body
                        }
                        .await;
                        out
                    })
                },
            );
            run(state, site, headers, None, request, $spec, f).await
        }
    };
    ($name:ident, id, $spec:expr, |$a:ident = $area:ident, $r:ident| $body:expr) => {
        async fn $name(
            State(state): State<RouteState>,
            site: SiteContext,
            headers: HeaderMap,
            Path(id): Path<String>,
            request: Request,
        ) -> Response {
            let f: Closure = Box::new(
                |port: Arc<dyn IdentityAdminPort>,
                 $r: Req|
                 -> BoxFuture<'static, Result<Reply, ApiError>> {
                    Box::pin(async move {
                        let out: Result<Reply, ApiError> = async {
                            let $a = port
                                .$area()
                                .ok_or_else(|| ApiError::from_code(Code::HttpInternal))?;
                            $body
                        }
                        .await;
                        out
                    })
                },
            );
            run(state, site, headers, Some(id), request, $spec.id(), f).await
        }
    };
}

use Area::{Assignments, Keys, Roles, Sessions, Users};
use IdentityPermission::{
    Assign, Impersonate, ImpersonationOwner, KeysManage, Manage, RolesManage,
};

const STATUSES: [&str; 4] = ["invited", "active", "disabled", "anonymised"];
const PAGING: &[&str] = &["cursor", "limit"];

op!(user_list, Spec::rd("identity_user_list", Users).q(&["status", "q", "cursor", "limit"]), |a = users, r| {
    let status = r.q.get("status").cloned();
    if status.as_deref().is_some_and(|s| !STATUSES.contains(&s)) {
        return Err(bad("`status` is not one of the allowed values."));
    }
    let q = r.q.get("q").cloned();
    if q.as_deref().is_some_and(|s| s.chars().count() > 100) {
        return Err(bad("`q` is at most 100 characters."));
    }
    let page = a.user_list(&r.ctx, UserListIn { status, q, page: page_of(&r.q)? }).await?;
    Ok(Reply::raw(page.json(|u| u.json())))
});

op!(user_create, Spec::wr("identity_user_create", Users, Manage).idem().body(), |a = users, r| {
    let m = closed(&r.body, &["email", "full_name", "audience", "roles", "invite"])?;
    let invite = match m.get("invite") {
        None => true,
        Some(v) => v.as_bool().ok_or_else(|| bad("`invite` must be true or false."))?,
    };
    let input = UserCreateIn {
        email: text(m, "email", 3, 320)?,
        full_name: text(m, "full_name", 1, 200)?,
        audience: one_of(m, "audience", &["staff", "partner"])?,
        roles: text_list(m, "roles", 20, 100)?,
        invite,
    };
    Ok(Reply::data(201, a.user_create(&r.ctx, input).await?.json()))
});

op!(user_get, id, Spec::rd("identity_user_get", Users), |a = users, r| {
    let user = a.user_get(&r.ctx, &r.id).await?;
    Ok(Reply::data(200, user.json()).etag(user.version))
});

op!(user_patch, id, Spec::wr("identity_user_patch", Users, Manage).ifm().body(), |a = users, r| {
    let m = closed(&r.body, &["full_name", "audience"])?;
    let patch = UserPatchIn {
        full_name: m.get("full_name").map(|_| text(m, "full_name", 1, 200)).transpose()?,
        audience: m.get("audience").map(|_| one_of(m, "audience", &["staff", "partner"])).transpose()?,
    };
    Ok(Reply::data(200, a.user_patch(&r.ctx, &r.id, r.version, patch).await?.json()))
});

op!(user_disable, id, Spec::wr("identity_user_disable", Users, Manage), |a = users, r| {
    Ok(Reply::data(200, a.user_disable(&r.ctx, &r.id).await?.json()))
});

op!(user_enable, id, Spec::wr("identity_user_enable", Users, Manage), |a = users, r| {
    Ok(Reply::data(200, a.user_enable(&r.ctx, &r.id).await?.json()))
});

op!(user_anonymise, id, Spec::wr("identity_user_anonymise", Users, Manage).body(), |a = users, r| {
    let m = closed(&r.body, &["confirm_email"])?;
    let confirm = text(m, "confirm_email", 1, 320)?;
    Ok(Reply::data(200, a.user_anonymise(&r.ctx, &r.id, confirm).await?.json()))
});

op!(user_recover, id, Spec::wr("identity_user_recover", Users, Manage).body(), |a = users, r| {
    let m = closed(&r.body, &["kind"])?;
    let kind = one_of(m, "kind", &["password_reset", "second_factor_reset"])?;
    match a.user_recover(&r.ctx, &r.id, kind).await? {
        RecoverOut::Sent | RecoverOut::NeedsApproval => Ok(Reply::status(202)),
    }
});

op!(invitation_list, Spec::rd("identity_invitation_list", Users).q(PAGING), |a = users, r| {
    Ok(Reply::raw(a.invitation_list(&r.ctx, page_of(&r.q)?).await?.json(|i| i.json())))
});

op!(invitation_revoke, id, Spec::wr("identity_invitation_revoke", Users, Manage), |a = users, r| {
    a.invitation_revoke(&r.ctx, &r.id).await?;
    Ok(Reply::status(204))
});

op!(invitation_resend, id, Spec::wr("identity_invitation_resend", Users, Manage), |a = users, r| {
    a.invitation_resend(&r.ctx, &r.id).await?;
    Ok(Reply::status(204))
});

op!(role_list, Spec::rd("identity_role_list", Roles).q(PAGING), |a = roles, r| {
    Ok(Reply::raw(a.role_list(&r.ctx, page_of(&r.q)?).await?.json(|x| x.json())))
});

op!(role_get, id, Spec::rd("identity_role_get", Roles), |a = roles, r| {
    Ok(Reply::data(200, a.role_get(&r.ctx, &r.id).await?.json()))
});

op!(role_customise, id, Spec::wr("identity_role_customise", Roles, RolesManage).body(), |a = roles, r| {
    let m = closed(&r.body, &["name"])?;
    let name = text(m, "name", 1, 100)?;
    Ok(Reply::data(201, a.role_customise(&r.ctx, &r.id, name).await?.json()))
});

op!(role_diff, id, Spec::rd("identity_role_upgrade_diff", Roles), |a = roles, r| {
    Ok(Reply::data(200, a.role_upgrade_diff(&r.ctx, &r.id).await?.json()))
});

op!(assignment_list, Spec::rd("identity_assignment_list", Assignments).q(&["user", "role", "cursor", "limit"]), |a = assignments, r| {
    let q = AssignmentListIn {
        user: opt_record_id(&r.q, "user")?,
        role: opt_record_id(&r.q, "role")?,
        page: page_of(&r.q)?,
    };
    Ok(Reply::raw(a.assignment_list(&r.ctx, q).await?.json(|x| x.json())))
});

op!(assignment_create, Spec::wr("identity_assignment_create", Assignments, Assign).idem().body(), |a = assignments, r| {
    let m = closed(&r.body, &["user_id", "role_id", "scope", "expires_at", "reason"])?;
    let input = AssignmentCreateIn {
        user_id: record_field(m, "user_id")?,
        role_id: record_field(m, "role_id")?,
        scope: opt_text(m, "scope", 200)?,
        expires_at: instant(m, "expires_at")?,
        reason: opt_text(m, "reason", 500)?,
    };
    match a.assignment_create(&r.ctx, input).await? {
        GrantOut::Granted(assignment) => Ok(Reply::data(201, json!({ "assignment": assignment.json() }))),
        GrantOut::Requested(id) => Ok(Reply::data(202, json!({ "grant_request_id": id }))),
    }
});

op!(assignment_delete, id, Spec::wr("identity_assignment_delete", Assignments, Assign), |a = assignments, r| {
    a.assignment_delete(&r.ctx, &r.id).await?;
    Ok(Reply::status(204))
});

op!(rules_get, Spec::rd("identity_assignment_rules_get", Assignments), |a = assignments, r| {
    let rules = a.rules_get(&r.ctx).await?;
    Ok(Reply::data(200, rules.json()).etag(rules.version))
});

op!(rules_put, Spec::wr("identity_assignment_rules_put", Assignments, Assign).ifm().body(), |a = assignments, r| {
    let rules = rules_of(&r.body)?;
    Ok(Reply::data(200, a.rules_put(&r.ctx, r.version, rules).await?.json()))
});

op!(rules_preview, Spec::wr("identity_assignment_rules_preview", Assignments, Assign).no_step().body(), |a = assignments, r| {
    let m = closed(&r.body, &["rules"])?;
    let rules = m.get("rules").map(rules_of).transpose()?;
    Ok(Reply::data(200, a.rules_preview(&r.ctx, rules).await?.json()))
});

op!(rules_run, Spec::wr("identity_assignment_rules_run", Assignments, Assign).idem().body(), |a = assignments, r| {
    let m = closed(&r.body, &["plan_digest"])?;
    let digest = text(m, "plan_digest", 1, 64)?;
    if !digest.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(bad("`plan_digest` is not a digest."));
    }
    let out = a.rules_run(&r.ctx, digest).await?;
    Ok(Reply::data(200, json!({"granted": out.granted, "revoked": out.revoked, "refused": out.refused})))
});

op!(api_key_list, Spec::rd("identity_api_key_list", Keys).q(PAGING), |a = keys, r| {
    Ok(Reply::raw(a.api_key_list(&r.ctx, page_of(&r.q)?).await?.json(|x| x.json())))
});

op!(api_key_create, Spec::wr("identity_api_key_create", Keys, KeysManage).idem().body(), |a = keys, r| {
    let m = closed(&r.body, &["name", "owner_id", "scopes", "expires_at"])?;
    let input = ApiKeyCreateIn {
        name: text(m, "name", 1, 100)?,
        owner_id: record_field(m, "owner_id")?,
        scopes: text_list(m, "scopes", 100, 200)?,
        expires_at: instant(m, "expires_at")?,
    };
    let created = a.api_key_create(&r.ctx, input).await?;
    Ok(Reply::data(201, created.json()))
});

op!(api_key_delete, id, Spec::wr("identity_api_key_delete", Keys, KeysManage), |a = keys, r| {
    a.api_key_delete(&r.ctx, &r.id).await?;
    Ok(Reply::status(204))
});

op!(scim_list, Spec::rd("identity_scim_credential_list", Keys).q(PAGING), |a = keys, r| {
    Ok(Reply::raw(a.scim_list(&r.ctx, page_of(&r.q)?).await?.json(|x| x.json())))
});

op!(scim_create, Spec::wr("identity_scim_credential_create", Keys, KeysManage).idem(), |a = keys, r| {
    Ok(Reply::data(201, a.scim_create(&r.ctx).await?.json()))
});

op!(scim_delete, id, Spec::wr("identity_scim_credential_delete", Keys, KeysManage), |a = keys, r| {
    a.scim_delete(&r.ctx, &r.id).await?;
    Ok(Reply::status(204))
});

op!(session_list, Spec::rd("identity_session_list", Sessions).q(&["user", "cursor", "limit"]), |a = sessions, r| {
    let user = opt_record_id(&r.q, "user")?.ok_or_else(|| bad("`user` is required."))?;
    let q = SessionListIn { user, page: page_of(&r.q)? };
    Ok(Reply::raw(a.session_list(&r.ctx, q).await?.json(|x| x.json())))
});

op!(session_end, id, Spec::wr("identity_session_end", Sessions, Manage), |a = sessions, r| {
    a.session_end(&r.ctx, &r.id).await?;
    Ok(Reply::status(204))
});

op!(sessions_end_all, id, Spec::wr("identity_user_sessions_end_all", Sessions, Manage), |a = sessions, r| {
    let ended = a.sessions_end_all(&r.ctx, &r.id).await?;
    Ok(Reply::data(200, json!({ "ended": ended })))
});

op!(impersonation_start, Spec::wr("identity_impersonation_start", Sessions, Impersonate).body(), |a = sessions, r| {
    let m = closed(&r.body, &["user_id", "reason", "ticket"])?;
    let input = ImpersonationIn {
        user_id: record_field(m, "user_id")?,
        reason: text(m, "reason", 1, 500)?,
        ticket: opt_text(m, "ticket", 100)?,
    };
    let expires_at = a.impersonation_start(&r.ctx, input).await?;
    Ok(Reply::data(200, json!({ "expires_at": expires_at })))
});

op!(impersonation_end, Spec::wr("identity_impersonation_end", Sessions, ImpersonationOwner).no_step(), |a = sessions, r| {
    a.impersonation_end(&r.ctx).await?;
    Ok(Reply::status(204))
});
