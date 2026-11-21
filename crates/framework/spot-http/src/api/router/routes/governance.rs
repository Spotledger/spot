//! The `/api/v1/governance/*` routes (contract 0.19.0, WP-CON-03.T42; WP-IAM-08.T8): grant
//! requests and decisions, segregation-of-duties rules, analysis and overrides, access reviews,
//! the four access questions and emergency access, as thin, bounded handlers over
//! [`GovernancePort`].
//!
//! # What the layer decides, and what it does not
//!
//! The routes read the **shape** of a request and refuse a malformed one before the port is
//! asked. They never decide who may do what: the port checks the platform permission (fail
//! closed), the tenant, self-approval, reviewer rights and segregation of duties, and writes the
//! audit record of each outcome. Specifically:
//!
//! - **Areas.** An area the host did not wire ([`GovernancePort::serves`]) answers
//!   `501 service.not_configured`; the others keep working. The group as a whole is not
//!   mounted without its port (the registry's unserved envelope).
//! - **Paging.** The query takes `cursor` (1 to 2048 characters) and `limit` (1 to 100, default
//!   50) and nothing the operation does not declare; the layer hands the port a digest of the
//!   operation, path ids and filters, and the port binds every cursor to the caller and that
//!   digest, so a replayed cursor of another user answers like an invalid one.
//! - **No actor in the body.** Bodies are objects with exactly the contract's fields: an
//!   unknown field (an `approver`, `reviewer`, `actor` or `tenant`, say) is refused. Reasons
//!   and notes are bounded (the contract's 500 characters, never more than 2 KiB) and passed
//!   through unchanged for the port to store; an error never echoes a value or an unknown key.
//! - **Step-up.** Every decision and every write the contract marks asks
//!   [`GovernancePort::step_up_current`] first: no recent step-up is `403 auth.step_up_required`
//!   and the operation is not called. (Creating a grant request has no step-up in the contract.)
//! - **Headers.** `Idempotency-Key` (1 to 128 visible ASCII characters) and `If-Match` (a quoted
//!   positive integer) are required exactly where the contract says. Every answer is
//!   `Cache-Control: private, no-store`.
//! - **Errors.** Only the registry codes of the contract are let through from the port;
//!   anything else is `http.internal` and is logged by code name only.
//!
//! # Registration
//!
//! Like `routes/export.rs`, this file does **not** declare the registry's `SERVES`, `routes`
//! and `required_ports`: registering would add 22 operations to the served set, and
//! `tests/conformance.rs` (not owned by this task) then needs a case for each. [`registration`]
//! hands the group to `routes::mount` for a host that mounts it itself; the thin registering
//! route file and the conformance cases are listed in the task report.

use std::collections::BTreeMap;

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
use crate::api::ports::governance::{
    GovernanceCall, GovernancePort, GovernanceReply, Op, PageRequest,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, ErrorCode, FieldIssue, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{build_ctx, render_error};

/// The operation ids this group serves once registered.
pub const GOVERNANCE_SERVES: &[&str] = &[
    "governance_grant_request_list",
    "governance_grant_request_create",
    "governance_grant_request_decide",
    "governance_sod_rules_get",
    "governance_sod_rules_put",
    "governance_sod_analysis",
    "governance_sod_override_list",
    "governance_sod_override_create",
    "governance_review_list",
    "governance_review_create",
    "governance_review_get",
    "governance_review_items",
    "governance_review_item_decide",
    "governance_review_close",
    "governance_review_report",
    "governance_question_user_permissions",
    "governance_question_who_can",
    "governance_question_who_can_see",
    "governance_question_changes",
    "governance_emergency_create",
    "governance_emergency_list",
    "governance_emergency_review",
];

/// The ports this group needs: the governance port, which carries the permission engine, the
/// tenant scope and the audit of every outcome. A group without it is not mounted at all.
pub fn governance_required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn GovernancePort>()];
    &KEYS
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "governance",
        serves: GOVERNANCE_SERVES,
        required_ports: governance_required_ports,
        routes: governance_routes,
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
    /// An array of at most `n` record ids.
    RecordIds(usize),
    /// The SoD rule set's `rules` array.
    SodRules,
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
const SEVERITY: &[&str] = &["low", "medium", "high"];

static GRANT_CREATE: [Field; 4] = [
    req("user_id", RECORD_ID),
    req("role_id", RECORD_ID),
    opt("scope", Kind::Text(0, 200)),
    req("reason", REASON),
];
static GRANT_DECIDE: [Field; 2] = [
    req("decision", Kind::OneOf(&["approve", "reject"])),
    req("reason", REASON),
];
static SOD_PUT: [Field; 2] = [
    req("rules", Kind::SodRules),
    opt("version", Kind::Int(1, i64::MAX)),
];
static OVERRIDE_CREATE: [Field; 6] = [
    req("rule_id", Kind::Text(1, 64)),
    req("user_id", RECORD_ID),
    opt("scope", Kind::Text(0, 200)),
    req("reason", REASON),
    req("mitigating_control", REASON),
    req("expires_at", Kind::DateTime),
];
static REVIEW_CREATE: [Field; 4] = [
    req("name", Kind::Text(1, 200)),
    opt("scope", Kind::Text(0, 500)),
    req("due_at", Kind::DateTime),
    opt(
        "reviewers",
        Kind::OneOf(&["manager", "role_owner", "named"]),
    ),
];
static ITEM_DECIDE: [Field; 2] = [
    req("decision", Kind::OneOf(&["keep", "remove"])),
    opt("comment", Kind::Text(0, 500)),
];
static EMERGENCY_CREATE: [Field; 4] = [
    req("reason", REASON),
    req("roles", Kind::RecordIds(20)),
    opt("duration_minutes", Kind::Int(1, 480)),
    req("controller_id", RECORD_ID),
];
static EMERGENCY_REVIEW: [Field; 2] = [
    req("outcome", Kind::OneOf(&["justified", "unjustified"])),
    opt("comment", Kind::Text(0, 500)),
];

static Q_STATUS: [Field; 1] = [opt(
    "status",
    Kind::OneOf(&["pending", "approved", "rejected", "withdrawn", "expired"]),
)];
static Q_RULE: [Field; 1] = [opt("rule", Kind::Text(1, 64))];
static Q_USER: [Field; 1] = [req("user", RECORD_ID)];
static Q_WHO_CAN: [Field; 2] = [
    req("permission", Kind::Text(1, 200)),
    opt("scope", Kind::Text(1, 200)),
];
static Q_WHO_CAN_SEE: [Field; 2] = [req("doctype", Kind::Text(1, 200)), req("id", RECORD_ID)];
static Q_CHANGES: [Field; 3] = [
    req("from", Kind::DateTime),
    req("to", Kind::DateTime),
    opt("user", RECORD_ID),
];

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
    created: bool,
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
        status: if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
    }
}

const GET: MethodFilter = MethodFilter::GET;
const POST: MethodFilter = MethodFilter::POST;
const PUT: MethodFilter = MethodFilter::PUT;

/// Every operation: method, path, filters, paging, body, step-up, `Idempotency-Key`, `If-Match`
/// and whether success is `201`.
#[rustfmt::skip]
static SPECS: [Spec; 22] = [
    spec( Op::GrantRequestList, GET, "/api/v1/governance/grant-requests", &Q_STATUS, true, None, false, false, false, false, ),
    spec( Op::GrantRequestCreate, POST, "/api/v1/governance/grant-requests", &[], false, Some(&GRANT_CREATE), false, true, false, true, ),
    spec( Op::GrantRequestDecide, POST, "/api/v1/governance/grant-requests/{id}/decide", &[], false, Some(&GRANT_DECIDE), true, true, false, false, ),
    spec( Op::SodRulesGet, GET, "/api/v1/governance/sod/rules", &[], false, None, false, false, false, false, ),
    spec( Op::SodRulesPut, PUT, "/api/v1/governance/sod/rules", &[], false, Some(&SOD_PUT), true, false, true, false, ),
    spec( Op::SodAnalysis, GET, "/api/v1/governance/sod/analysis", &Q_RULE, true, None, false, false, false, false, ),
    spec( Op::SodOverrideList, GET, "/api/v1/governance/sod/overrides", &[], true, None, false, false, false, false, ),
    spec( Op::SodOverrideCreate, POST, "/api/v1/governance/sod/overrides", &[], false, Some(&OVERRIDE_CREATE), true, true, false, true, ),
    spec( Op::ReviewList, GET, "/api/v1/governance/access-reviews", &[], true, None, false, false, false, false, ),
    spec( Op::ReviewCreate, POST, "/api/v1/governance/access-reviews", &[], false, Some(&REVIEW_CREATE), true, true, false, true, ),
    spec( Op::ReviewGet, GET, "/api/v1/governance/access-reviews/{id}", &[], false, None, false, false, false, false, ),
    spec( Op::ReviewItems, GET, "/api/v1/governance/access-reviews/{id}/items", &[], true, None, false, false, false, false, ),
    spec( Op::ReviewItemDecide, POST, "/api/v1/governance/access-reviews/{id}/items/{item}/decide", &[], false, Some(&ITEM_DECIDE), true, true, false, false, ),
    spec( Op::ReviewClose, POST, "/api/v1/governance/access-reviews/{id}/close", &[], false, None, true, false, false, false, ),
    spec( Op::ReviewReport, GET, "/api/v1/governance/access-reviews/{id}/report", &[], false, None, false, false, false, false, ),
    spec( Op::QuestionUserPermissions, GET, "/api/v1/governance/questions/user-permissions", &Q_USER, true, None, false, false, false, false, ),
    spec( Op::QuestionWhoCan, GET, "/api/v1/governance/questions/who-can", &Q_WHO_CAN, true, None, false, false, false, false, ),
    spec( Op::QuestionWhoCanSee, GET, "/api/v1/governance/questions/who-can-see", &Q_WHO_CAN_SEE, true, None, false, false, false, false, ),
    spec( Op::QuestionChanges, GET, "/api/v1/governance/questions/changes", &Q_CHANGES, true, None, false, false, false, false, ),
    spec( Op::EmergencyCreate, POST, "/api/v1/governance/emergency-access", &[], false, Some(&EMERGENCY_CREATE), true, true, false, true, ),
    spec( Op::EmergencyList, GET, "/api/v1/governance/emergency-access", &[], true, None, false, false, false, false, ),
    spec( Op::EmergencyReview, POST, "/api/v1/governance/emergency-access/{id}/review", &[], false, Some(&EMERGENCY_REVIEW), true, false, false, false, ),
];

/// The group's routes: one per operation, methods on one path merged.
pub fn governance_routes() -> Router<RouteState> {
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

/// The codes the port may answer with.
const PORT_CODES: &[Code] = &[
    Code::AuthRequired,
    Code::AuthSessionExpired,
    Code::AuthStepUpRequired,
    Code::PermissionDenied,
    Code::GovernanceNotFound,
    Code::GovernanceInvalid,
    Code::GovernanceSelfApproval,
    Code::GovernanceRequestNotPending,
    Code::GovernanceOverrideInvalid,
    Code::GovernanceReviewClosed,
    Code::GovernanceNotReviewer,
    Code::GovernanceControllerIsBeneficiary,
    Code::IdentitySodConflict,
    Code::DocumentNotFound,
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
        tracing::error!("the governance port answered with a code outside the contract");
        code_failure(Code::HttpInternal, request_id)
    }
}

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site does not serve this part of access governance.",
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
        Kind::RecordIds(_) | Kind::SodRules => false,
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

fn check_rules(value: &Json) -> bool {
    let Some(rules) = value.as_array() else {
        return false;
    };
    rules.len() <= 200
        && rules.iter().all(|rule| {
            let Some(rule) = rule.as_object() else {
                return false;
            };
            let known = |k: &String| {
                ["id", "name", "roles", "kind", "severity", "enabled"].contains(&k.as_str())
            };
            rule.keys().all(known)
                && rule
                    .get("id")
                    .and_then(Json::as_str)
                    .is_some_and(|s| text_ok(s, 1, 64))
                && rule
                    .get("name")
                    .and_then(Json::as_str)
                    .is_some_and(|s| text_ok(s, 0, 200))
                && rule.get("roles").and_then(Json::as_array).is_some_and(|r| {
                    r.len() <= 20 && r.iter().all(|x| x.as_str().is_some_and(is_record_id))
                })
                && matches!(
                    rule.get("kind").and_then(Json::as_str),
                    Some("exclusive" | "limit")
                )
                && rule
                    .get("severity")
                    .is_none_or(|s| s.as_str().is_some_and(|s| SEVERITY.contains(&s)))
                && rule.get("enabled").is_some_and(Json::is_boolean)
        })
}

fn value_ok(kind: Kind, value: &Json) -> bool {
    match kind {
        Kind::Int(min, max) => value.as_i64().is_some_and(|n| (min..=max).contains(&n)),
        Kind::RecordIds(max) => value.as_array().is_some_and(|l| {
            l.len() <= max && l.iter().all(|x| x.as_str().is_some_and(is_record_id))
        }),
        Kind::SodRules => check_rules(value),
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

fn reply_response(spec: &Spec, reply: &GovernanceReply, request_id: &RequestId) -> Response {
    let mut body = Map::new();
    body.insert("data".to_owned(), reply.data.clone());
    match (&reply.next_cursor, spec.paged) {
        (Some(Some(cursor)), true) if (1..=2048).contains(&cursor.len()) => {
            body.insert("next_cursor".to_owned(), Json::String(cursor.clone()));
        }
        (Some(None), true) => {
            body.insert("next_cursor".to_owned(), Json::Null);
        }
        (None, false) => {}
        _ => {
            tracing::error!("the governance port answered outside the contract's paging rule");
            return code_failure(Code::HttpInternal, request_id);
        }
    }
    let mut response = (spec.status, axum::Json(Json::Object(body))).into_response();
    if let Some(etag) = &reply.etag {
        match HeaderValue::from_str(etag) {
            Ok(value) if etag_ok(etag) => {
                response.headers_mut().insert(header::ETAG, value);
            }
            _ => {
                tracing::error!("the governance port returned an ETag outside the contract");
                return code_failure(Code::HttpInternal, request_id);
            }
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
    let Some(port) = state.port::<dyn GovernancePort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if !port.serves(spec.op.area()) {
        return not_configured(request_id);
    }
    let ctx: RequestCtx = build_ctx(&site, &headers, None);
    if ctx.credential.is_none() {
        return code_failure(Code::AuthRequired, request_id);
    }
    // Path ids: visible ASCII, 1 to 200; anything else is the same answer as a missing one.
    if ids.iter().any(|id| !visible_ascii(id, 200)) {
        return code_failure(Code::GovernanceNotFound, request_id);
    }
    let (filters, page) = match parse_query(spec, request.uri().query().unwrap_or("")) {
        Ok(parsed) => parsed,
        Err(error) => return failure(error, request_id),
    };
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
            let object = match read_object(request).await {
                Ok(object) => object,
                Err(error) => return failure(error, request_id),
            };
            if let Err(error) = check_body(fields, &object) {
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
        match port.step_up_current(&ctx).await {
            Ok(true) => {}
            Ok(false) => return code_failure(Code::AuthStepUpRequired, request_id),
            // An error from the step-up check is a refusal, never an allow.
            Err(error) => return port_failure(error, request_id),
        }
    }
    let call = GovernanceCall {
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
    match port.call(&ctx, &call).await {
        Ok(reply) => reply_response(spec, &reply, request_id),
        Err(error) => port_failure(error, request_id),
    }
}
