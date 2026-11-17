//! The audit and support-access routes under `/api/v1/security/` (contract 0.20.0,
//! WP-CON-03.T43; WP-SEC-07.T3): audit chain verification, masked entries and export, the
//! compliance evidence pack and controls, support-access requests, consent, revocation, support
//! sessions and recordings, as thin, bounded handlers over [`AuditPort`] and
//! [`SupportAccessPort`].
//!
//! # What the layer decides, and what it does not
//!
//! The routes read the **shape** of a request and refuse a malformed one before a port is
//! asked. The ports decide who may read an audit or a recording (the platform permission, fail
//! closed), the tenant scope, the masking, the verification of a consent, and audit every
//! outcome; the layer asserts it did not bypass them and adds no data of its own to an answer.
//!
//! - **An unset port is `501 service.not_configured`;** the group is mounted only when both
//!   ports are present (the registry's unserved envelope).
//! - **Paging.** Entries, controls, requests, sessions and recordings take `cursor` (1 to 2048
//!   characters) and `limit` (1 to 100, default 50); the layer hands the port a digest of the
//!   operation, path ids and filters and the port binds each cursor to caller and digest, so a
//!   replayed cursor of another user answers like an invalid one.
//! - **Export.** The window is `from` < `to` and at most 31 days (`422 security.invalid`
//!   otherwise, before the port is called); the answer is at most 1 MiB; `Content-Type`
//!   (`application/x-ndjson`) and `Content-Disposition` (`attachment; filename="audit-export.ndjson"`)
//!   are fixed by the layer and `X-Content-Type-Options: nosniff` is set.
//! - **Recordings.** `Cache-Control: private, no-store` and a fixed JSON `Content-Type`, never
//!   taken from stored data; each row must be exactly `at`, `action`, `target` with the
//!   contract's bounds or the answer is refused (`500 http.internal`), so stored text cannot
//!   smuggle markup or extra fields through the layer.
//! - **Consent.** The body is exactly `consent` (at most 8192 characters) and `signature` (a
//!   non-empty base64 value of at most 1024 characters, alphabet and padding checked here,
//!   **verified by the port**); an unknown field is refused and never echoed; the approving
//!   user is the authenticated user, never a body field; the signature is never returned (the
//!   layer refuses an answer with a field named like a signature, secret or key).
//! - **Step-up** is asked of the port before consent, revoke, recordings and the export; no
//!   recent step-up is `403 auth.step_up_required` and the operation is not called.
//! - **Bulk class.** Chain verification, export and evidence packs are the contract's `bulk`
//!   rate class; the layer has no limiter for authenticated groups yet, so the port limits
//!   them (see the report).
//! - **Errors.** Only the contract's codes are let through from a port; anything else is
//!   `http.internal`.
//!
//! # Registration
//!
//! Like `routes/export.rs` this file does **not** declare the registry's `SERVES`, `routes`
//! and `required_ports` (registering adds 10 operations and `tests/conformance.rs`, not owned
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
use crate::api::ports::security_audit::{
    AuditCall, AuditPort, AuditReply, Op, PageRequest, SupportAccessPort,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, ErrorCode, FieldIssue, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{build_ctx, render_error};

/// The operation ids this group serves once registered.
pub const SECURITY_AUDIT_SERVES: &[&str] = &[
    "security_audit_chain_verify",
    "security_audit_entries",
    "security_audit_export",
    "security_compliance_evidence_pack",
    "security_compliance_controls",
    "security_support_request_list",
    "security_support_request_consent",
    "security_support_request_revoke",
    "security_support_session_list",
    "security_support_session_recording",
];

/// The ports this group needs: both carry the permission engine, the tenant scope, the masking
/// and the audit of every outcome. A group without either is not mounted at all.
pub fn security_audit_required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 2] = [
        PortKey::of::<dyn AuditPort>(),
        PortKey::of::<dyn SupportAccessPort>(),
    ];
    &KEYS
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "security_audit",
        serves: SECURITY_AUDIT_SERVES,
        required_ports: security_audit_required_ports,
        routes: security_audit_routes,
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
    /// An integer in the range.
    Int(i64, i64),
    /// An RFC 3339 date-time, at most 40 characters.
    DateTime,
    /// Any text of `min..=max` characters without a NUL (a signed consent record).
    Blob(usize, usize),
    /// A non-empty base64 value (standard or URL-safe alphabet, padding optional) of at most
    /// `n` characters.
    Base64(usize),
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

static VERIFY: [Field; 2] = [opt("from", Kind::DateTime), opt("to", Kind::DateTime)];
static PACK: [Field; 3] = [
    req("framework", Kind::Text(1, 60)),
    opt("from", Kind::DateTime),
    opt("to", Kind::DateTime),
];
static CONSENT: [Field; 2] = [
    req("consent", Kind::Blob(1, 8192)),
    req("signature", Kind::Base64(1024)),
];
static Q_ENTRIES: [Field; 3] = [
    opt("from", Kind::DateTime),
    opt("to", Kind::DateTime),
    opt("actor", RECORD_ID),
];
static Q_EXPORT: [Field; 2] = [req("from", Kind::DateTime), req("to", Kind::DateTime)];

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

#[rustfmt::skip]
static SPECS: [Spec; 10] = [
    spec(Op::AuditChainVerify, POST, "/api/v1/security/audit/chain/verify", &[], false, Some(&VERIFY), false, false, false, 200),
    spec(Op::AuditEntries, GET, "/api/v1/security/audit/entries", &Q_ENTRIES, true, None, false, false, false, 200),
    spec(Op::AuditExport, GET, "/api/v1/security/audit/export", &Q_EXPORT, false, None, true, false, false, 200),
    spec(Op::ComplianceEvidencePack, POST, "/api/v1/security/compliance/evidence-pack", &[], false, Some(&PACK), false, true, false, 202),
    spec(Op::ComplianceControls, GET, "/api/v1/security/compliance/controls", &[], true, None, false, false, false, 200),
    spec(Op::SupportRequestList, GET, "/api/v1/security/support-access/requests", &[], true, None, false, false, false, 200),
    spec(Op::SupportRequestConsent, POST, "/api/v1/security/support-access/requests/{id}/consent", &[], false, Some(&CONSENT), true, false, false, 200),
    spec(Op::SupportRequestRevoke, POST, "/api/v1/security/support-access/requests/{id}/revoke", &[], false, None, true, false, false, 200),
    spec(Op::SupportSessionList, GET, "/api/v1/security/support-access/sessions", &[], true, None, false, false, false, 200),
    spec(Op::SupportSessionRecording, GET, "/api/v1/security/support-access/sessions/{id}/recording", &[], true, None, true, false, false, 200),
];

/// The group's routes: one per operation, methods on one path merged.
pub fn security_audit_routes() -> Router<RouteState> {
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
    Code::SecurityConsentInvalid,
    Code::ApiInvalidRequest,
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
        Kind::DateTime => is_date_time(value),
        Kind::Int(min, max) => {
            !value.is_empty()
                && value.bytes().all(|b| b.is_ascii_digit())
                && value.parse::<i64>().is_ok_and(|n| (min..=max).contains(&n))
        }
        Kind::Blob(..) | Kind::Base64(_) => false,
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

fn base64_ok(value: &str, max: usize) -> bool {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    (1..=max).contains(&value.len())
        && [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD]
            .iter()
            .any(|engine| engine.decode(value).is_ok())
}

fn value_ok(kind: Kind, value: &Json) -> bool {
    match kind {
        Kind::Int(min, max) => value.as_i64().is_some_and(|n| (min..=max).contains(&n)),
        Kind::Blob(min, max) => value.as_str().is_some_and(|s| {
            (min..=max).contains(&s.chars().count()) && !s.contains('\0') && s.len() <= 4 * max
        }),
        Kind::Base64(max) => value.as_str().is_some_and(|s| base64_ok(s, max)),
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

const MAX_EXPORT_BYTES: usize = 1_048_576;

fn parsed(filters: &BTreeMap<String, String>, name: &str) -> Option<OffsetDateTime> {
    filters
        .get(name)
        .and_then(|v| OffsetDateTime::parse(v, &Rfc3339).ok())
}

fn ordered(from: Option<OffsetDateTime>, to: Option<OffsetDateTime>) -> bool {
    match (from, to) {
        (Some(from), Some(to)) => from <= to,
        _ => true,
    }
}

/// Cross-field rules of the filters: a window is ordered; the export window is at most 31 days.
fn check_filters(op: Op, filters: &BTreeMap<String, String>) -> Result<(), ApiError> {
    let (from, to) = (parsed(filters, "from"), parsed(filters, "to"));
    if !ordered(from, to) {
        return Err(ApiError::new(
            Code::SecurityInvalid,
            "The window must start before it ends.",
        ));
    }
    if op == Op::AuditExport
        && let (Some(from), Some(to)) = (from, to)
        && to - from > time::Duration::days(31)
    {
        return Err(ApiError::new(
            Code::SecurityInvalid,
            "An export window is at most 31 days.",
        ));
    }
    Ok(())
}

/// The rules that need more than one field: a window in a body is ordered.
fn check_special(_op: Op, body: &mut Map<String, Json>) -> Result<(), ApiError> {
    let read = |name: &str| {
        body.get(name)
            .and_then(Json::as_str)
            .and_then(|v| OffsetDateTime::parse(v, &Rfc3339).ok())
    };
    if !ordered(read("from"), read("to")) {
        return Err(ApiError::new(
            Code::SecurityInvalid,
            "The window must start before it ends.",
        ));
    }
    Ok(())
}

/// The port of an operation: the support-access port answers five of them.
enum Target {
    Audit(Arc<dyn AuditPort>),
    Support(Arc<dyn SupportAccessPort>),
}

impl Target {
    fn resolve(state: &RouteState, op: Op) -> Option<Target> {
        if op.is_support_access() {
            state.port::<dyn SupportAccessPort>().map(Target::Support)
        } else {
            state.port::<dyn AuditPort>().map(Target::Audit)
        }
    }

    async fn step_up(&self, ctx: &RequestCtx) -> Result<bool, ApiError> {
        match self {
            Target::Audit(port) => port.step_up_current(ctx).await,
            Target::Support(port) => port.step_up_current(ctx).await,
        }
    }

    async fn call(&self, ctx: &RequestCtx, call: &AuditCall) -> Result<AuditReply, ApiError> {
        match self {
            Target::Audit(port) => port.call(ctx, call).await,
            Target::Support(port) => port.call(ctx, call).await,
        }
    }
}

/// A recording row is exactly `at`, `action` and `target`, within the contract's bounds.
fn recording_rows_ok(data: &Json) -> bool {
    let Some(rows) = data.as_array() else {
        return false;
    };
    rows.len() <= 100
        && rows.iter().all(|row| {
            let Some(row) = row.as_object() else {
                return false;
            };
            row.len() == 3
                && row
                    .get("at")
                    .and_then(Json::as_str)
                    .is_some_and(is_date_time)
                && row
                    .get("action")
                    .and_then(Json::as_str)
                    .is_some_and(|s| s.chars().count() <= 200)
                && row.get("target").is_some_and(|t| {
                    t.is_null() || t.as_str().is_some_and(|s| s.chars().count() <= 500)
                })
        })
}

fn reply_response(spec: &Spec, reply: &AuditReply, request_id: &RequestId) -> Response {
    let outside = |what: &str| {
        tracing::error!("the security port answered outside the contract: {what}");
        code_failure(Code::HttpInternal, request_id)
    };
    if spec.op == Op::AuditExport {
        let Some(lines) = &reply.export else {
            return outside("no export");
        };
        if reply.data.is_some() || reply.next_cursor.is_some() || lines.len() > MAX_EXPORT_BYTES {
            return outside("an export outside its bounds");
        }
        let mut response = (StatusCode::OK, lines.clone()).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson"),
        );
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"audit-export.ndjson\""),
        );
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        return finish(response);
    }
    if reply.export.is_some() {
        return outside("an export on another operation");
    }
    let Some(data) = &reply.data else {
        return outside("no data");
    };
    if carries_forbidden_field(data, 0) {
        return outside("a field named like a secret, key or signature");
    }
    if spec.op == Op::SupportSessionRecording && !recording_rows_ok(data) {
        return outside("a recording row");
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
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
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
    let call = AuditCall {
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
