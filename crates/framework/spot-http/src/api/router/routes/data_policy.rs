//! The data-policy and protected-group routes under `/api/v1/security/` (contract 0.20.0,
//! WP-CON-03.T43; WP-SEC-05.T6): the policy, retention runs, holds, proofs, the residency
//! check, protected groups, sync classes and node-key revocation, as thin, bounded handlers over
//! [`DataPolicyPort`] and [`ProtectedGroupPort`].
//!
//! # What the layer decides, and what it does not
//!
//! The routes read the **shape** of a request and refuse a malformed one before a port is
//! asked. The ports decide who may do what (the platform permission, fail closed), the tenant
//! scope, the digest check and the key rotation, and audit every outcome. Specifically:
//!
//! - **An unset port is `501 service.not_configured`.** The group is mounted only when both
//!   ports are present (the registry's unserved envelope); a host that mounts it by hand with
//!   one port still gets the 501 for the operations of the missing one.
//! - **A real retention run needs its digest at the edge too.** `POST retention/runs` with
//!   `dry_run: false` and no `plan_digest` is refused (`422 security.plan_required`, the
//!   contract's code; the brief said 400) **before the port is called**, so a port bug cannot
//!   turn a preview into a deletion; a dry run may not carry a digest; `dry_run` defaults to
//!   `true`; the digest is 64 lower-case hex characters.
//! - **No key material, ever.** The layer refuses (`500 http.internal`) any port answer whose
//!   JSON has a field named like key material (`secret`, `seed`, `wrapped`, `private`,
//!   `material`, `signature`, a bare `key` or a `*_key`); the contract's `key_version` and
//!   public `fingerprint` pass. No request body accepts such a field either.
//! - **Revoke and sync-class writes are never replayed and never cached.** They take no
//!   `Idempotency-Key` (a header that is sent is ignored, never passed on) and every response
//!   is `Cache-Control: private, no-store`.
//! - **Step-up.** Every write asks `step_up_current` of the port first: no recent step-up is
//!   `403 auth.step_up_required` and the operation is not called.
//! - **Paging, bodies, headers, errors.** A cursor is bound to the caller and the query through
//!   a digest the port must honour; bodies are exact-field objects (an unknown field is refused
//!   and never echoed, text is bounded to 2 KiB, a body to 64 KiB); only the contract's codes
//!   are let through from a port.
//!
//! # Registration
//!
//! Like `routes/export.rs` this file does **not** declare the registry's `SERVES`, `routes`
//! and `required_ports` (registering adds 13 operations and `tests/conformance.rs`, not owned
//! by this task, then needs a case for each). [`registration`] hands the group to
//! `routes::mount` for a host that mounts it itself.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{RawPathParams, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter, on};
use serde_json::{Map, Value as Json, json};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::data_policy::{
    DataPolicyPort, Op, PageRequest, PolicyCall, PolicyReply, ProtectedGroupPort,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, ErrorCode, FieldIssue, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{build_ctx, render_error};

/// The operation ids this group serves once registered.
pub const SECURITY_DATA_POLICY_SERVES: &[&str] = &[
    "security_data_policy_get",
    "security_data_policy_put",
    "security_retention_run_list",
    "security_retention_run_create",
    "security_retention_hold_list",
    "security_retention_hold_create",
    "security_retention_hold_delete",
    "security_retention_proof_list",
    "security_residency_check",
    "security_protected_group_list",
    "security_protected_group_sync_class",
    "security_node_key_list",
    "security_node_key_revoke",
];

/// The ports this group needs: both carry the permission engine, the tenant scope and the audit
/// of every outcome. A group without either is not mounted at all.
pub fn data_policy_required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 2] = [
        PortKey::of::<dyn DataPolicyPort>(),
        PortKey::of::<dyn ProtectedGroupPort>(),
    ];
    &KEYS
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "data_policy",
        serves: SECURITY_DATA_POLICY_SERVES,
        required_ports: data_policy_required_ports,
        routes: data_policy_routes,
        public: None,
    }
}

// --- the table of operations --------------------------------------------------------------------

/// The kind of a bounded value.
#[derive(Clone, Copy)]
enum Kind {
    /// Text of `min..=max` characters (no control characters but line breaks and tabs).
    Text(usize, usize),
    /// A canonical lower-case UUIDv7.
    RecordId,
    /// One of the listed strings.
    OneOf(&'static [&'static str]),
    /// An integer in the range.
    Int(i64, i64),
    /// An RFC 3339 date-time, at most 40 characters.
    DateTime,
    /// An array of at most `n` strings of at most `m` characters each.
    TextList(usize, usize),
    /// A boolean.
    Bool,
    /// Lower-case hex of exactly 64 characters (a SHA-256 plan digest).
    Digest,
    /// The policy's `classes` array.
    Classes,
}

#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    kind: Kind,
    required: bool,
}

const fn req(name: &'static str, kind: Kind) -> Field {
    Field {
        name,
        kind,
        required: true,
    }
}

const fn opt(name: &'static str, kind: Kind) -> Field {
    Field {
        name,
        kind,
        required: false,
    }
}

#[derive(Clone, Copy)]
struct Spec {
    op: Op,
    method: MethodFilter,
    path: &'static str,
    /// The filters (not cursor and limit).
    query: &'static [Field],
    paged: bool,
    /// `Some` when the operation takes a JSON body with exactly these fields.
    body: Option<&'static [Field]>,
    step_up: bool,
    key: bool,
    if_match: bool,
    status: StatusCode,
}

const RECORD_ID: Kind = Kind::RecordId;
const REASON: Kind = Kind::Text(1, 500);

static POLICY_PUT: [Field; 4] = [
    req("region", Kind::Text(1, 60)),
    opt("residency", Kind::TextList(20, 60)),
    req("classes", Kind::Classes),
    opt("version", Kind::Int(1, i64::MAX)),
];
static RUN_CREATE: [Field; 2] = [opt("dry_run", Kind::Bool), opt("plan_digest", Kind::Digest)];
static HOLD_CREATE: [Field; 3] = [
    req("scope", REASON),
    req("reason", REASON),
    opt("expires_at", Kind::DateTime),
];
static RESIDENCY: [Field; 2] = [
    req("region", Kind::Text(1, 60)),
    opt("doctypes", Kind::TextList(100, 200)),
];
static SYNC_CLASS: [Field; 2] = [
    req(
        "sync_class",
        Kind::OneOf(&["everywhere", "hub_only", "named_nodes"]),
    ),
    opt("nodes", Kind::TextList(100, 100)),
];
static Q_RUN: [Field; 1] = [opt("run", RECORD_ID)];

#[allow(clippy::too_many_arguments)]
const fn spec(
    op: Op,
    method: MethodFilter,
    path: &'static str,
    query: &'static [Field],
    paged: bool,
    body: Option<&'static [Field]>,
    step_up: bool,
    key: bool,
    if_match: bool,
    status: u16,
) -> Spec {
    Spec {
        op,
        method,
        path,
        query,
        paged,
        body,
        step_up,
        key,
        if_match,
        status: match status {
            201 => StatusCode::CREATED,
            202 => StatusCode::ACCEPTED,
            204 => StatusCode::NO_CONTENT,
            _ => StatusCode::OK,
        },
    }
}

const GET: MethodFilter = MethodFilter::GET;
const POST: MethodFilter = MethodFilter::POST;
const PUT: MethodFilter = MethodFilter::PUT;
const DELETE: MethodFilter = MethodFilter::DELETE;

#[rustfmt::skip]
static SPECS: [Spec; 13] = [
    spec(Op::DataPolicyGet, GET, "/api/v1/security/data-policy", &[], false, None, false, false, false, 200),
    spec(Op::DataPolicyPut, PUT, "/api/v1/security/data-policy", &[], false, Some(&POLICY_PUT), true, false, true, 200),
    spec(Op::RetentionRunList, GET, "/api/v1/security/retention/runs", &[], true, None, false, false, false, 200),
    spec(Op::RetentionRunCreate, POST, "/api/v1/security/retention/runs", &[], false, Some(&RUN_CREATE), true, true, false, 202),
    spec(Op::RetentionHoldList, GET, "/api/v1/security/retention/holds", &[], true, None, false, false, false, 200),
    spec(Op::RetentionHoldCreate, POST, "/api/v1/security/retention/holds", &[], false, Some(&HOLD_CREATE), true, true, false, 201),
    spec(Op::RetentionHoldDelete, DELETE, "/api/v1/security/retention/holds/{id}", &[], false, None, true, false, false, 204),
    spec(Op::RetentionProofList, GET, "/api/v1/security/retention/proofs", &Q_RUN, true, None, false, false, false, 200),
    spec(Op::ResidencyCheck, POST, "/api/v1/security/residency/check", &[], false, Some(&RESIDENCY), false, false, false, 200),
    spec(Op::ProtectedGroupList, GET, "/api/v1/security/protected-groups", &[], true, None, false, false, false, 200),
    spec(Op::ProtectedGroupSyncClass, PUT, "/api/v1/security/protected-groups/{group}/sync-class", &[], false, Some(&SYNC_CLASS), true, false, false, 200),
    spec(Op::NodeKeyList, GET, "/api/v1/security/node-keys", &[], true, None, false, false, false, 200),
    spec(Op::NodeKeyRevoke, POST, "/api/v1/security/node-keys/{node}/revoke", &[], false, None, true, false, false, 200),
];

/// The group's routes: one per operation, methods on one path merged.
pub fn data_policy_routes() -> Router<RouteState> {
    let mut by_path: BTreeMap<&'static str, MethodRouter<RouteState>> = BTreeMap::new();
    for spec in &SPECS {
        let spec: &'static Spec = spec;
        let handler = move |State(state): State<RouteState>,
                            site: SiteContext,
                            params: RawPathParams,
                            headers: HeaderMap,
                            request: Request| {
            let ids: Vec<String> = params.iter().map(|(_, v)| v.to_owned()).collect();
            async move { dispatch(spec, state, site, ids, headers, request).await }
        };
        let method_router = on(spec.method, handler);
        let merged = match by_path.remove(spec.path) {
            Some(existing) => existing.merge(method_router),
            None => method_router,
        };
        by_path.insert(spec.path, merged);
    }
    by_path
        .into_iter()
        .fold(Router::new(), |router, (path, methods)| {
            router.route(path, methods)
        })
}

// --- helpers ----------------------------------------------------------------------------------

const MAX_BODY_BYTES: usize = 64 * 1024;

const MAX_TEXT_BYTES: usize = 2048;

/// The codes a port may answer with.
const PORT_CODES: &[Code] = &[
    Code::AuthRequired,
    Code::AuthSessionExpired,
    Code::AuthStepUpRequired,
    Code::PermissionDenied,
    Code::SecurityNotFound,
    Code::SecurityInvalid,
    Code::SecurityPlanRequired,
    Code::SecurityPlanChanged,
    Code::DocumentStaleVersion,
    Code::ApiInvalidRequest,
    Code::ApiPreconditionRequired,
    Code::ApiIdempotencyConflict,
    Code::ApiRateLimited,
    Code::ServerUnavailable,
];

fn finish(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn failure(error: ApiError, request_id: &RequestId) -> Response {
    finish(render_error(error, request_id))
}

fn code_failure(code: Code, request_id: &RequestId) -> Response {
    failure(ApiError::from_code(code), request_id)
}

fn port_failure(error: ApiError, request_id: &RequestId) -> Response {
    if matches!(&error.code, ErrorCode::Registry(c) if PORT_CODES.contains(c)) {
        failure(error, request_id)
    } else {
        tracing::error!("the security port answered with a code outside the contract");
        code_failure(Code::HttpInternal, request_id)
    }
}

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site does not serve this part of security administration.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    finish(response)
}

fn invalid(field: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, "The request is not valid.").with_field(FieldIssue::new(
        field,
        "api.invalid_request",
        "This value is not valid.",
    ))
}

fn visible_ascii(value: &str, max: usize) -> bool {
    (1..=max).contains(&value.len()) && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn is_record_id(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            14 => *c == b'7',
            19 => matches!(c, b'8' | b'9' | b'a' | b'b'),
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

fn is_date_time(value: &str) -> bool {
    value.len() <= 40 && OffsetDateTime::parse(value, &Rfc3339).is_ok()
}

fn text_ok(value: &str, min: usize, max: usize) -> bool {
    let chars = value.chars().count();
    (min..=max).contains(&chars)
        && value.len() <= MAX_TEXT_BYTES
        && !value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = raw.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Whether a query or path-less string value of `kind` is acceptable.
fn scalar_ok(kind: Kind, value: &str) -> bool {
    match kind {
        Kind::Text(min, max) => text_ok(value, min, max),
        Kind::RecordId => is_record_id(value),
        Kind::OneOf(list) => list.contains(&value),
        Kind::DateTime => is_date_time(value),
        Kind::Int(min, max) => {
            !value.is_empty()
                && value.bytes().all(|b| b.is_ascii_digit())
                && value.parse::<i64>().is_ok_and(|n| (min..=max).contains(&n))
        }
        Kind::TextList(..) | Kind::Bool | Kind::Classes => false,
        Kind::Digest => {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
    }
}

/// The filters and the page of `query`, or the field that is wrong. Each name at most once;
/// a name the operation does not declare is refused.
#[allow(clippy::type_complexity)]
fn parse_query(
    spec: &Spec,
    raw: &str,
) -> Result<(BTreeMap<String, String>, Option<(Option<String>, u32)>), ApiError> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key).ok_or_else(|| invalid("$"))?;
        let known = spec.query.iter().any(|f| f.name == key)
            || (spec.paged && (key == "cursor" || key == "limit"));
        if !known {
            return Err(invalid("$"));
        }
        let value = percent_decode(value).ok_or_else(|| invalid(&key))?;
        if seen.insert(key.clone(), value).is_some() {
            return Err(invalid(&key));
        }
    }
    let mut filters = BTreeMap::new();
    for field in spec.query {
        match seen.remove(field.name) {
            Some(value) if scalar_ok(field.kind, &value) => {
                filters.insert(field.name.to_owned(), value);
            }
            Some(_) => return Err(invalid(field.name)),
            None if field.required => return Err(invalid(field.name)),
            None => {}
        }
    }
    let page = if spec.paged {
        let cursor = match seen.remove("cursor") {
            Some(c) if (1..=2048).contains(&c.len()) => Some(c),
            Some(_) => return Err(invalid("cursor")),
            None => None,
        };
        let limit = match seen.remove("limit") {
            Some(l) if scalar_ok(Kind::Int(1, 100), &l) => {
                l.parse::<u32>().map_err(|_| invalid("limit"))?
            }
            Some(_) => return Err(invalid("limit")),
            None => 50,
        };
        Some((cursor, limit))
    } else {
        None
    };
    Ok((filters, page))
}

fn classes_ok(value: &Json) -> bool {
    let Some(list) = value.as_array() else {
        return false;
    };
    list.len() <= 50
        && list.iter().all(|item| {
            let Some(item) = item.as_object() else {
                return false;
            };
            item.keys()
                .all(|k| ["class", "retention_days", "legal_hold_allowed"].contains(&k.as_str()))
                && item
                    .get("class")
                    .and_then(Json::as_str)
                    .is_some_and(|s| text_ok(s, 1, 60))
                && item.get("retention_days").is_some_and(|d| {
                    d.is_null() || d.as_i64().is_some_and(|n| (1..=36500).contains(&n))
                })
                && item.get("legal_hold_allowed").is_some_and(Json::is_boolean)
        })
}

fn value_ok(kind: Kind, value: &Json) -> bool {
    match kind {
        Kind::Int(min, max) => value.as_i64().is_some_and(|n| (min..=max).contains(&n)),
        Kind::Bool => value.is_boolean(),
        Kind::TextList(items, chars) => value.as_array().is_some_and(|l| {
            l.len() <= items
                && l.iter()
                    .all(|x| x.as_str().is_some_and(|s| text_ok(s, 1, chars)))
        }),
        Kind::Classes => classes_ok(value),
        other => value.as_str().is_some_and(|s| scalar_ok(other, s)),
    }
}

/// A body object with exactly the declared fields. The error names a declared field path or
/// `$`; an unknown key is never echoed.
fn check_body(fields: &[Field], body: &Map<String, Json>) -> Result<(), ApiError> {
    if body.keys().any(|k| !fields.iter().any(|f| f.name == k)) {
        return Err(invalid("$"));
    }
    for field in fields {
        match body.get(field.name) {
            Some(value) if value_ok(field.kind, value) => {}
            Some(_) => return Err(invalid(field.name)),
            None if field.required => return Err(invalid(field.name)),
            None => {}
        }
    }
    Ok(())
}

fn media_type(headers: &HeaderMap) -> Option<String> {
    let mut all = headers.get_all(header::CONTENT_TYPE).iter();
    let first = all.next()?;
    if all.next().is_some() {
        return None;
    }
    let text = first.to_str().ok()?;
    Some(
        text.split(';')
            .next()
            .unwrap_or(text)
            .trim()
            .to_ascii_lowercase(),
    )
}

async fn read_object(request: Request) -> Result<Map<String, Json>, ApiError> {
    let (parts, body) = request.into_parts();
    if media_type(&parts.headers).as_deref() != Some("application/json") {
        return Err(ApiError::from_code(Code::ApiUnsupportedMediaType));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::from_code(Code::HttpBodyTooLarge))?;
    match serde_json::from_slice::<Json>(&bytes) {
        Ok(Json::Object(map)) => Ok(map),
        Ok(_) => Err(ApiError::new(
            Code::ApiInvalidRequest,
            "The body must be a JSON object.",
        )),
        Err(_) => Err(ApiError::from_code(Code::ApiInvalidJson)),
    }
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let mut all = headers.get_all("idempotency-key").iter();
    let value = all.next()?.to_str().ok()?;
    if all.next().is_some() || !visible_ascii(value, 128) {
        return None;
    }
    Some(value.to_owned())
}

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
        && if_match(&{
            let mut h = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(etag) {
                h.insert(header::IF_MATCH, v);
            }
            h
        })
        .is_some()
}

/// A digest of the operation, the path ids and the filters (an unambiguous JSON encoding).
fn query_digest(op: Op, path: &[String], filters: &BTreeMap<String, String>) -> String {
    let encoded = serde_json::to_string(&(op.id(), path, filters)).unwrap_or_default();
    let digest = Sha256::digest(encoded.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Names a response field may never have: key material, a wrapped key, a seed, a signature, a
/// secret. `key_version` and a public `fingerprint` are the contract's and pass.
fn forbidden_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "key"
        || n.ends_with("_key")
        || [
            "secret",
            "seed",
            "wrapped",
            "private",
            "material",
            "signature",
            "password",
            "token",
        ]
        .iter()
        .any(|bad| n.contains(bad))
}

/// Whether `value` carries a forbidden field name anywhere (depth-bounded: deeper is refused).
fn carries_forbidden_field(value: &Json, depth: usize) -> bool {
    if depth > 16 {
        return true;
    }
    match value {
        Json::Object(map) => map
            .iter()
            .any(|(k, v)| forbidden_name(k) || carries_forbidden_field(v, depth + 1)),
        Json::Array(items) => items.iter().any(|v| carries_forbidden_field(v, depth + 1)),
        _ => false,
    }
}

/// The rules that need more than one field. A real retention run carries its plan digest at the
/// edge too: `dry_run: false` without `plan_digest` is `422 security.plan_required` and the port
/// is never called; a dry run carries no digest; `dry_run` defaults to `true`.
fn check_special(op: Op, body: &mut Map<String, Json>) -> Result<(), ApiError> {
    if op != Op::RetentionRunCreate {
        return Ok(());
    }
    let dry_run = body.get("dry_run").and_then(Json::as_bool).unwrap_or(true);
    let has_digest = body.contains_key("plan_digest");
    match (dry_run, has_digest) {
        (true, false) => {
            body.insert("dry_run".to_owned(), Json::Bool(true));
            Ok(())
        }
        (true, true) => Err(invalid("plan_digest")),
        (false, false) => Err(ApiError::from_code(Code::SecurityPlanRequired)),
        (false, true) => Ok(()),
    }
}

/// Cross-field rules of the filters: none in this group.
fn check_filters(_op: Op, _filters: &BTreeMap<String, String>) -> Result<(), ApiError> {
    Ok(())
}

/// The port of an operation: the protected-group port answers four of them.
enum Target {
    Policy(Arc<dyn DataPolicyPort>),
    Groups(Arc<dyn ProtectedGroupPort>),
}

impl Target {
    fn resolve(state: &RouteState, op: Op) -> Option<Target> {
        if op.is_protected_group() {
            state.port::<dyn ProtectedGroupPort>().map(Target::Groups)
        } else {
            state.port::<dyn DataPolicyPort>().map(Target::Policy)
        }
    }

    async fn step_up(&self, ctx: &RequestCtx) -> Result<bool, ApiError> {
        match self {
            Target::Policy(port) => port.step_up_current(ctx).await,
            Target::Groups(port) => port.step_up_current(ctx).await,
        }
    }

    async fn call(&self, ctx: &RequestCtx, call: &PolicyCall) -> Result<PolicyReply, ApiError> {
        match self {
            Target::Policy(port) => port.call(ctx, call).await,
            Target::Groups(port) => port.call(ctx, call).await,
        }
    }
}

fn reply_response(spec: &Spec, reply: &PolicyReply, request_id: &RequestId) -> Response {
    let outside = |what: &str| {
        tracing::error!("the security port answered outside the contract: {what}");
        code_failure(Code::HttpInternal, request_id)
    };
    if spec.status == StatusCode::NO_CONTENT {
        if reply.data.is_some() || reply.next_cursor.is_some() || reply.etag.is_some() {
            return outside("a body on a 204");
        }
        return finish(StatusCode::NO_CONTENT.into_response());
    }
    let Some(data) = &reply.data else {
        return outside("no data");
    };
    if carries_forbidden_field(data, 0) {
        return outside("a field named like key material");
    }
    let mut body = Map::new();
    body.insert("data".to_owned(), data.clone());
    match (&reply.next_cursor, spec.paged) {
        (Some(Some(cursor)), true) if (1..=2048).contains(&cursor.len()) => {
            body.insert("next_cursor".to_owned(), Json::String(cursor.clone()));
        }
        (Some(None), true) => {
            body.insert("next_cursor".to_owned(), Json::Null);
        }
        (None, false) => {}
        _ => return outside("the paging rule"),
    }
    let mut response = (spec.status, axum::Json(Json::Object(body))).into_response();
    if let Some(etag) = &reply.etag {
        match HeaderValue::from_str(etag) {
            Ok(value) if etag_ok(etag) => {
                response.headers_mut().insert(header::ETAG, value);
            }
            _ => return outside("an ETag"),
        }
    }
    finish(response)
}

// --- the handler ------------------------------------------------------------------------------

async fn dispatch(
    spec: &'static Spec,
    state: RouteState,
    site: SiteContext,
    ids: Vec<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = &site.request_id;
    let Some(target) = Target::resolve(&state, spec.op) else {
        return not_configured(request_id);
    };
    let ctx: RequestCtx = build_ctx(&site, &headers, None);
    if ctx.credential.is_none() {
        return code_failure(Code::AuthRequired, request_id);
    }
    // Path ids: visible ASCII, 1 to 200; anything else is the same answer as a missing one.
    if ids.iter().any(|id| !visible_ascii(id, 200)) {
        return code_failure(Code::SecurityNotFound, request_id);
    }
    let (filters, page) = match parse_query(spec, request.uri().query().unwrap_or("")) {
        Ok(parsed) => parsed,
        Err(error) => return failure(error, request_id),
    };
    if let Err(error) = check_filters(spec.op, &filters) {
        return failure(error, request_id);
    }
    let key = if spec.key {
        match idempotency_key(&headers) {
            Some(key) => Some(key),
            None => {
                return failure(
                    ApiError::new(
                        Code::ApiInvalidRequest,
                        "An Idempotency-Key header is required.",
                    ),
                    request_id,
                );
            }
        }
    } else {
        None
    };
    let version = if spec.if_match {
        match if_match(&headers) {
            Some(version) => Some(version),
            None => return code_failure(Code::ApiPreconditionRequired, request_id),
        }
    } else {
        None
    };
    let body = match spec.body {
        Some(fields) => {
            let mut object = match read_object(request).await {
                Ok(object) => object,
                Err(error) => return failure(error, request_id),
            };
            if let Err(error) = check_body(fields, &object) {
                return failure(error, request_id);
            }
            if let Err(error) = check_special(spec.op, &mut object) {
                return failure(error, request_id);
            }
            Some(object)
        }
        None => None,
    };
    let ctx = RequestCtx {
        idempotency_key: key.clone(),
        ..ctx
    };
    if spec.step_up {
        match target.step_up(&ctx).await {
            Ok(true) => {}
            Ok(false) => return code_failure(Code::AuthStepUpRequired, request_id),
            // An error from the step-up check is a refusal, never an allow.
            Err(error) => return port_failure(error, request_id),
        }
    }
    let call = PolicyCall {
        op: spec.op,
        page: page.map(|(cursor, limit)| PageRequest {
            cursor,
            limit,
            query_digest: query_digest(spec.op, &ids, &filters),
        }),
        path: ids,
        query: filters,
        body,
        if_match: version,
        idempotency_key: key,
    };
    match target.call(&ctx, &call).await {
        Ok(reply) => reply_response(spec, &reply, request_id),
        Err(error) => port_failure(error, request_id),
    }
}
