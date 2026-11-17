//! The process version and migration operations (native API 0.12.0, WP-BPM-09.T3):
//! `process_version_list`, `process_version_publish`, `process_version_get`,
//! `process_version_retire`, `process_migration_plan`, `process_migration_start` and
//! `process_migration_get`.
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the **shape** of a request (path ids, query, the closed JSON body, the
//! `Idempotency-Key`) and refuse a malformed one before the port is asked. They never decide who
//! the caller is, which permission the caller holds or whether a definition is valid: the
//! [`VersionPort`] answers for the caller its credential names and the routes pass it **no user,
//! no role and no tenant**. The tenant is the site the request resolved to. Every body and every
//! query is closed, so a field that names an actor is refused as unknown; no header is read for
//! one. `definition` goes to the port untouched.
//!
//! # Fail closed
//!
//! - A group without its ports is not mounted (the registry). A host that mounts the routes over
//!   a state with no [`VersionPort`] gets `501 service.not_configured`, never `200` with an empty
//!   list.
//! - A port error outside the declared refusals, and a port answer outside the contract (a page
//!   longer than asked, a hash that is not 64 hex digits, a problem with free text), is
//!   `500 http.internal` with no detail, logged without content.
//! - "Not there" and "not yours" are one port error, rendered the same way every time.
//!
//! # Keys, caching and the body cap
//!
//! Publish and migration start require an `Idempotency-Key`; the route keeps the key protocol
//! through the [`IdempotencyStore`] port (registered groups sit outside the command middleware):
//! the first request answers `201` or `202`, a replay of the same key and body answers `200`
//! with the first answer and `Idempotency-Replayed: true`, the same key with another body is
//! `409 api.idempotency_conflict`. The scope is the site and the credential; only the success is
//! stored. `process_version_get` carries `ETag` (the content hash) and `Cache-Control: no-store`
//! and honours `If-None-Match` with `304` once the port allowed the read; every other answer is
//! `private, no-store`. The publish body is read while it streams, never past
//! [`PUBLISH_BODY_CAP`]. A refused request is logged with the operation id and the code only.

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value as Json, json};

use super::RouteState;
use crate::api::idempotency::{IdempotencyStore, KeyScope, fingerprint};
use crate::api::ports::PortKey;
use crate::api::ports::process_version::{
    DEFAULT_PAGE, MAX_DEFINITION_BYTES, MAX_NODE_COUNTS, MAX_NOTE_CHARS, MAX_PAGE, MAX_PROBLEMS,
    MAX_UNMOVED, MigrationMapping, MigrationPlan, MigrationProgress, MigrationRequest, Problem,
    PublishRequest, VersionDetail, VersionPort, VersionPortError, VersionSummary,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::middleware::{
    Admission, CLASS_HTTP_API, Outcome, Persist, ReservationGuard, admit, replay_response, settle,
};
use super::super::{Failure, build_ctx, credential_hash, fail, matches_etag};

/// The operation ids this group serves once registered.
pub const SERVES: &[&str] = &[
    "process_version_list",
    "process_version_publish",
    "process_version_get",
    "process_version_retire",
    "process_migration_plan",
    "process_migration_start",
    "process_migration_get",
];

/// The ports this group needs: the version port (whose adapter decides identity, permission,
/// validation and the audit) and the idempotency store of the two keyed writes.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 2] = [
        PortKey::of::<dyn VersionPort>(),
        PortKey::of::<dyn IdempotencyStore>(),
    ];
    &KEYS
}

/// The routes. The version segment is `{n}` in all of them: the router allows one name per
/// segment (the contract says `{from}` on the migration routes) and handlers read it by position.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route(
            "/api/v1/process/{process}/versions",
            get(version_list).post(version_publish),
        )
        .route("/api/v1/process/{process}/versions/{n}", get(version_get))
        .route(
            "/api/v1/process/{process}/versions/{n}/retire",
            post(version_retire),
        )
        .route(
            "/api/v1/process/{process}/versions/{n}/migrations/plan",
            post(migration_plan),
        )
        .route(
            "/api/v1/process/{process}/versions/{n}/migrations",
            post(migration_start),
        )
        .route(
            "/api/v1/process/{process}/migrations/{id}",
            get(migration_get),
        )
}

/// The largest publish body read: three times the definition's 256 KiB, for the escapes a JSON
/// string needs (a quote or newline doubles, a control character takes six) and the envelope.
/// The decoded definition is held to 256 KiB separately.
pub const PUBLISH_BODY_CAP: usize = 3 * MAX_DEFINITION_BYTES;
/// The largest migration body read: 500 node pairs, 100 paths and 64 KiB of defaults, with room.
const MIGRATION_BODY_CAP: usize = 384 * 1024;
const MAX_ID: usize = 128;
const MAX_CURSOR_LEN: usize = 2048;

// ------------------------------------------------------------------------------------------
// Answers

/// Why a handler stopped: an error to render, or a ready answer (`501`).
enum Stop {
    Fail(Failure),
    Respond(Box<Response>),
}

impl IntoResponse for Stop {
    fn into_response(self) -> Response {
        match self {
            Stop::Fail(failure) => failure.into_response(),
            Stop::Respond(response) => *response,
        }
    }
}

type Handled = Result<Response, Stop>;

/// Every answer is non-cacheable by a shared cache; `version_get` sets its own stricter value.
fn done(result: Handled) -> Response {
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    if !response.headers().contains_key(header::CACHE_CONTROL) {
        let value = HeaderValue::from_static("private, no-store");
        response.headers_mut().insert(header::CACHE_CONTROL, value);
    }
    response
}

/// A refusal: logged with the operation id and the problem code only, then rendered.
fn refuse(operation: &'static str, error: ApiError, request_id: &RequestId) -> Stop {
    tracing::info!(operation, code = error.code.as_str(), "request refused");
    Stop::Fail(fail(error, request_id))
}

fn invalid(operation: &'static str, message: &str, request_id: &RequestId) -> Stop {
    refuse(
        operation,
        ApiError::new(Code::ApiInvalidRequest, message),
        request_id,
    )
}

/// A port that answered outside the contract: a service bug, reported without detail.
fn violation(operation: &'static str, request_id: &RequestId) -> Stop {
    tracing::error!(operation, "the version port answered outside the contract");
    Stop::Fail(fail(ApiError::from_code(Code::HttpInternal), request_id))
}

/// `501 service.not_configured`: the site has no process version service.
fn not_configured(request_id: &RequestId) -> Stop {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no process version service configured.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    let json_type = HeaderValue::from_static("application/json");
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, json_type);
    Stop::Respond(Box::new(response))
}

/// Whether `raw` is a closed fault token: a stable dotted code of at most 64 characters.
fn is_kind_token(raw: &str) -> bool {
    raw.len() <= 64
        && raw.split('.').all(|segment| {
            let bytes = segment.as_bytes();
            bytes.first().is_some_and(u8::is_ascii_lowercase)
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        })
}

/// The problems of a refused definition as the contract's `details.problems`: node ids and
/// closed kinds only. Anything else is a port bug: `None`.
fn problems_json(problems: &[Problem]) -> Option<Json> {
    let node_ok = |node: &str| {
        (1..=MAX_ID).contains(&node.len())
            && node
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
    };
    if problems.is_empty() || problems.len() > MAX_PROBLEMS {
        return None;
    }
    let mut out = Vec::with_capacity(problems.len());
    for problem in problems {
        if !is_kind_token(&problem.kind) || problem.node.as_deref().is_some_and(|n| !node_ok(n)) {
            return None;
        }
        out.push(match &problem.node {
            Some(node) => json!({"node": node, "kind": problem.kind}),
            None => json!({"kind": problem.kind}),
        });
    }
    Some(Json::Array(out))
}

/// A port refusal as the contract's error: only the declared codes, and no detail but the
/// problems of an invalid definition. `missing` is the code of `NotFound` on this operation.
fn port_failure(op: &'static str, error: VersionPortError, missing: Code, rid: &RequestId) -> Stop {
    let api_error = match error {
        VersionPortError::NotFound => ApiError::from_code(missing),
        VersionPortError::PublishDenied => ApiError::from_code(Code::ProcessPublishDenied),
        VersionPortError::VersionConflict => ApiError::from_code(Code::ProcessVersionConflict),
        VersionPortError::DefinitionInvalid(problems) => {
            let Some(listed) = problems_json(&problems) else {
                return violation(op, rid);
            };
            let mut api_error = ApiError::from_code(Code::ProcessDefinitionInvalid);
            api_error.details.insert("problems".to_owned(), listed);
            api_error
        }
        VersionPortError::VersionNotPublished => {
            ApiError::from_code(Code::ProcessVersionNotPublished)
        }
        VersionPortError::MigrationInvalid => ApiError::from_code(Code::ProcessMigrationInvalid),
        VersionPortError::BadCursor => ApiError::new(
            Code::ApiInvalidRequest,
            "`cursor` is not one this service issued.",
        ),
        VersionPortError::Unavailable => ApiError::from_code(Code::ServerUnavailable),
        VersionPortError::Internal => {
            tracing::error!(op, "the version port failed");
            ApiError::from_code(Code::HttpInternal)
        }
    };
    refuse(op, api_error, rid)
}

// ------------------------------------------------------------------------------------------
// The call and its parts

/// The port and the request context of an authenticated call.
struct Call {
    port: Arc<dyn VersionPort>,
    ctx: RequestCtx,
}

/// The start of every handler: the caller must bring a credential (`401`), the group must have
/// its port (`501`), and the process name of the path must be a storage name (`400`).
fn begin(
    op: &'static str,
    state: &RouteState,
    site: &SiteContext,
    headers: &HeaderMap,
    process: &str,
) -> Result<Call, Stop> {
    let rid = &site.request_id;
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(refuse(op, ApiError::from_code(Code::AuthRequired), rid));
    }
    let Some(port) = state.port::<dyn VersionPort>() else {
        return Err(not_configured(rid));
    };
    let bytes = process.as_bytes();
    let name_ok = (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
    if !name_ok {
        return Err(invalid(op, "A process name must be a storage name.", rid));
    }
    Ok(Call { port, ctx })
}

/// A version number of the path: canonical, digits only, no sign, no leading zero, 1 to 9 digits.
fn version_of(op: &'static str, raw: &str, rid: &RequestId) -> Result<u32, Stop> {
    let canonical = (1..=9).contains(&raw.len())
        && raw.bytes().all(|b| b.is_ascii_digit())
        && !raw.starts_with('0');
    let parsed = canonical.then(|| raw.parse::<u32>().ok()).flatten();
    parsed.ok_or_else(|| invalid(op, "A version number must be a whole number from 1.", rid))
}

/// A migration id: `^[A-Za-z0-9_-]{16,64}$`.
fn is_opaque_id(raw: &str) -> bool {
    (16..=64).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The query of a list or a read: `cursor` and `limit` only (`allow` false: nothing), each at
/// most once. A cursor is opaque and URL-safe, so it is never percent-decoded.
fn parse_query(
    op: &'static str,
    raw: Option<&str>,
    allow: bool,
    rid: &RequestId,
) -> Result<(Option<String>, u32), Stop> {
    let (mut cursor, mut limit) = (None, None);
    for pair in raw.unwrap_or_default().split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let taken = match (allow, key) {
            (true, "cursor") => {
                let url_safe = |b: u8| b.is_ascii_alphanumeric() || b"_-.~=".contains(&b);
                let ok = (1..=MAX_CURSOR_LEN).contains(&value.len()) && value.bytes().all(url_safe);
                ok.then(|| cursor.replace(value.to_owned()))
                    .map(|old| old.is_none())
            }
            (true, "limit") => {
                let parsed = value
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| value.parse::<u32>());
                let n = parsed
                    .and_then(Result::ok)
                    .filter(|n| (1..=MAX_PAGE).contains(n));
                n.map(|n| limit.replace(n).is_none())
            }
            _ => None,
        };
        if taken != Some(true) {
            return Err(invalid(op, "The query is not valid for this call.", rid));
        }
    }
    Ok((cursor, limit.unwrap_or(DEFAULT_PAGE)))
}

/// The `Idempotency-Key`: required, exactly once, 1 to 128 visible ASCII characters.
fn idempotency_key(op: &'static str, headers: &HeaderMap, rid: &RequestId) -> Result<String, Stop> {
    let mut values = headers.get_all("idempotency-key").iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err(invalid(
            op,
            "An Idempotency-Key header is required, exactly once.",
            rid,
        ));
    };
    match value.to_str() {
        Ok(key)
            if (1..=128).contains(&key.len())
                && key.bytes().all(|b| (0x21..=0x7e).contains(&b)) =>
        {
            Ok(key.to_owned())
        }
        _ => Err(invalid(
            op,
            "The Idempotency-Key must be 1 to 128 visible ASCII characters.",
            rid,
        )),
    }
}

/// Reads a body of at most `cap` bytes: the media type, the declared and the real size are all
/// checked, and the body is never buffered past `cap` (the read stops when it is exceeded).
async fn read_body(
    op: &'static str,
    request: Request,
    cap: usize,
    rid: &RequestId,
) -> Result<Vec<u8>, Stop> {
    let (parts, body) = request.into_parts();
    let header_text = |name| parts.headers.get(name).and_then(|v| v.to_str().ok());
    let media = header_text(header::CONTENT_TYPE)
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    if media.is_some_and(|m| m != "application/json") {
        return Err(refuse(
            op,
            ApiError::from_code(Code::ApiUnsupportedMediaType),
            rid,
        ));
    }
    let declared = header_text(header::CONTENT_LENGTH).and_then(|v| v.trim().parse::<usize>().ok());
    let too_large = || refuse(op, ApiError::from_code(Code::HttpBodyTooLarge), rid);
    if declared.is_some_and(|n| n > cap) {
        return Err(too_large());
    }
    let bytes = axum::body::to_bytes(body, cap)
        .await
        .map_err(|_| too_large())?;
    Ok(bytes.to_vec())
}

/// A JSON object body whose keys are all in `allowed` (the contract's bodies are closed).
fn closed_object(
    op: &'static str,
    bytes: &[u8],
    allowed: &[&str],
    rid: &RequestId,
) -> Result<Map<String, Json>, Stop> {
    let parsed = if bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<Json>(bytes).ok()
    };
    let Some(parsed) = parsed else {
        return Err(refuse(op, ApiError::from_code(Code::ApiInvalidJson), rid));
    };
    match parsed {
        Json::Object(map) if map.keys().all(|key| allowed.contains(&key.as_str())) => Ok(map),
        _ => Err(invalid(
            op,
            "The request body must be an object of the documented properties.",
            rid,
        )),
    }
}

/// A whole number from `min` that fits a `u32`; a float, a string or a negative number is not.
fn whole_number(value: &Json, min: u32) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n >= min)
}

// ------------------------------------------------------------------------------------------
// The bodies

fn publish_body(op: &'static str, bytes: &[u8], rid: &RequestId) -> Result<PublishRequest, Stop> {
    let mut map = closed_object(op, bytes, &["definition", "note", "expected_previous"], rid)?;
    let definition = match map.remove("definition") {
        Some(Json::String(text)) if !text.is_empty() && text.len() <= MAX_DEFINITION_BYTES => text,
        _ => {
            return Err(invalid(
                op,
                "`definition` is required, at most 256 KiB.",
                rid,
            ));
        }
    };
    let note = match map.remove("note") {
        None => String::new(),
        Some(Json::String(text)) if text.chars().count() <= MAX_NOTE_CHARS => text,
        Some(_) => {
            return Err(invalid(
                op,
                "`note` must be text of at most 500 characters.",
                rid,
            ));
        }
    };
    let expected_previous = match map.remove("expected_previous") {
        None => None,
        Some(value) => match whole_number(&value, 0) {
            Some(n) => Some(n),
            None => {
                return Err(invalid(
                    op,
                    "`expected_previous` must be a whole number.",
                    rid,
                ));
            }
        },
    };
    Ok(PublishRequest {
        definition,
        note,
        expected_previous,
    })
}

/// The contract's `MigrationMapping`: closed, every list and map bounded.
fn mapping_of(op: &'static str, value: Json, rid: &RequestId) -> Result<MigrationMapping, Stop> {
    let bad = || invalid(op, "`mapping` is not in the documented form.", rid);
    let text = |v: &Json, max: usize| {
        v.as_str()
            .filter(|s| (1..=max).contains(&s.len()))
            .map(str::to_owned)
    };
    let Json::Object(mut map) = value else {
        return Err(bad());
    };
    if map
        .keys()
        .any(|k| !["nodes", "required_variables", "variable_defaults"].contains(&k.as_str()))
    {
        return Err(bad());
    }
    let mut mapping = MigrationMapping::default();
    if let Some(nodes) = map.remove("nodes") {
        let Json::Object(nodes) = nodes else {
            return Err(bad());
        };
        if nodes.len() > 500 {
            return Err(bad());
        }
        for (from, to) in &nodes {
            match (from.len() <= MAX_ID && !from.is_empty(), text(to, MAX_ID)) {
                (true, Some(to)) => mapping.nodes.insert(from.clone(), to),
                _ => return Err(bad()),
            };
        }
    }
    if let Some(paths) = map.remove("required_variables") {
        let Json::Array(paths) = paths else {
            return Err(bad());
        };
        if paths.len() > 100 {
            return Err(bad());
        }
        for path in &paths {
            mapping
                .required_variables
                .push(text(path, 256).ok_or_else(bad)?);
        }
    }
    if let Some(defaults) = map.remove("variable_defaults") {
        let Json::Object(defaults) = defaults else {
            return Err(bad());
        };
        let fits = serde_json::to_vec(&defaults).is_ok_and(|b| b.len() <= 64 * 1024);
        if defaults.len() > 100 || !fits {
            return Err(bad());
        }
        mapping.variable_defaults = defaults;
    }
    Ok(mapping)
}

fn migration_body(
    op: &'static str,
    bytes: &[u8],
    rid: &RequestId,
) -> Result<MigrationRequest, Stop> {
    let mut map = closed_object(op, bytes, &["to", "mapping"], rid)?;
    let to = map.remove("to").and_then(|value| whole_number(&value, 1));
    let Some(to) = to else {
        return Err(invalid(
            op,
            "`to` is required: a version number from 1.",
            rid,
        ));
    };
    let mapping = map
        .remove("mapping")
        .map(|value| mapping_of(op, value, rid))
        .transpose()?;
    Ok(MigrationRequest { to, mapping })
}

// ------------------------------------------------------------------------------------------
// JSON of the contract's schemas

/// The contract's `ProcessVersion` fields, or `None` when the port's version is outside the
/// contract's bounds.
fn version_fields(version: &VersionSummary) -> Option<Map<String, Json>> {
    let stamp_ok = |stamp: &Option<String>| {
        stamp
            .as_deref()
            .is_none_or(|s| s.ends_with('Z') && s.len() <= 40)
    };
    let by_ok = version
        .published_by
        .as_deref()
        .is_none_or(|b| (1..=MAX_ID).contains(&b.len()));
    if version.version == 0
        || version.note.chars().count() > MAX_NOTE_CHARS
        || !stamp_ok(&version.published_at)
        || !by_ok
    {
        return None;
    }
    let Json::Object(map) = json!({
        "version": version.version, "state": version.state.as_str(),
        "published_at": version.published_at, "published_by": version.published_by,
        "note": version.note, "running_runs": version.running_runs,
    }) else {
        return None;
    };
    Some(map)
}

fn is_hash(raw: &str) -> bool {
    raw.len() == 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn detail_json(detail: &VersionDetail) -> Option<Json> {
    let mut map = version_fields(&detail.version)?;
    if !is_hash(&detail.hash) || detail.definition.len() > MAX_DEFINITION_BYTES {
        return None;
    }
    map.insert(
        "definition".to_owned(),
        Json::from(detail.definition.clone()),
    );
    map.insert("hash".to_owned(), Json::from(detail.hash.clone()));
    Some(Json::Object(map))
}

fn plan_json(plan: &MigrationPlan) -> Option<Json> {
    let node_ok = |node: &str| (1..=MAX_ID).contains(&node.len());
    let reason_ok = |reason: &str| reason.contains('.') && is_kind_token(reason);
    if plan.node_counts.len() > MAX_NODE_COUNTS
        || plan.unmoved.len() > MAX_UNMOVED
        || plan.from == 0
        || plan.to == 0
        || plan
            .node_counts
            .iter()
            .any(|c| !node_ok(&c.from) || !node_ok(&c.to))
        || plan
            .unmoved
            .iter()
            .any(|u| !is_opaque_id(&u.run) || !reason_ok(&u.reason))
    {
        return None;
    }
    let counts: Vec<Json> = plan
        .node_counts
        .iter()
        .map(|c| json!({"from": c.from, "to": c.to, "runs": c.runs}))
        .collect();
    let unmoved: Vec<Json> = plan
        .unmoved
        .iter()
        .map(|u| json!({"run": u.run, "reason": u.reason}))
        .collect();
    Some(json!({
        "from": plan.from, "to": plan.to, "movable": plan.movable,
        "unmoved_total": plan.unmoved_total, "node_counts": counts, "unmoved": unmoved,
    }))
}

fn progress_json(progress: &MigrationProgress) -> Option<Json> {
    let stamp_ok = |stamp: &str| stamp.ends_with('Z') && stamp.len() <= 40;
    if !stamp_ok(&progress.started_at) || progress.ended_at.as_deref().is_some_and(|e| !stamp_ok(e))
    {
        return None;
    }
    let mut body = json!({
        "state": progress.state.as_str(), "moved": progress.moved,
        "skipped": progress.skipped, "failed": progress.failed, "started_at": progress.started_at,
    });
    if let (Some(ended), Some(map)) = (&progress.ended_at, body.as_object_mut()) {
        map.insert("ended_at".to_owned(), Json::from(ended.clone()));
    }
    Some(body)
}

fn json_response(status: StatusCode, body: Json) -> Response {
    (status, axum::Json(body)).into_response()
}

fn etag_value(hash: &str) -> Option<HeaderValue> {
    is_hash(hash)
        .then(|| HeaderValue::from_str(&format!("\"{hash}\"")).ok())
        .flatten()
}

// ------------------------------------------------------------------------------------------
// The keyed writes

/// The store, scope and fingerprint of a keyed write. The scope is the site and the credential,
/// never across them; the fingerprint covers the method, the path and the caller too.
fn key_parts(
    state: &RouteState,
    site: &SiteContext,
    call: &Call,
    key: String,
    path: &str,
    bytes: &[u8],
) -> Result<(Arc<dyn IdempotencyStore>, KeyScope, String), Stop> {
    let Some(store) = state.port::<dyn IdempotencyStore>() else {
        // Never write without the key's reservation.
        return Err(not_configured(&site.request_id));
    };
    let credential = credential_hash(call.ctx.credential.as_ref());
    let site_name = site.site.as_str().to_owned();
    let scope = KeyScope {
        site: site_name,
        credential: credential.clone(),
        key,
    };
    let print = fingerprint("POST", &format!("{path}\n{credential}"), bytes);
    Ok((store, scope, print))
}

/// Runs `command` under the key protocol: reserve, run, settle. A replay answers `200` with the
/// first answer; only `success` is kept, every refusal releases the key.
async fn keyed<F>(
    op: &'static str,
    keys: (Arc<dyn IdempotencyStore>, KeyScope, String),
    success: StatusCode,
    rid: &RequestId,
    command: F,
) -> Handled
where
    F: Future<Output = Handled>,
{
    let (store, scope, print) = keys;
    match admit(&*store, &scope, &print).await {
        Err(error) => return Err(refuse(op, error, rid)),
        Ok(Admission::Conflict) => {
            return Err(refuse(
                op,
                ApiError::from_code(Code::ApiIdempotencyConflict),
                rid,
            ));
        }
        Ok(Admission::Replay(stored)) => {
            // The contract's replay is `200`, whatever the first answer's status was.
            let mut response = replay_response(&stored, rid);
            *response.status_mut() = StatusCode::OK;
            return Ok(response);
        }
        Ok(Admission::Run) => {}
    }
    // Releases the reservation if this future is dropped before the outcome is recorded.
    let mut guard = ReservationGuard {
        store: Arc::clone(&store),
        scope: scope.clone(),
        fingerprint: print.clone(),
        armed: true,
    };
    let response = command.await.unwrap_or_else(IntoResponse::into_response);
    let status = response.status();
    let (parts, body) = response.into_parts();
    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let outcome = Outcome {
        persist: Persist::Body,
        class: CLASS_HTTP_API,
        release: status != success,
    };
    settle(&*store, &scope, &print, outcome, &parts, &body_bytes).await;
    guard.armed = false;
    Ok(Response::from_parts(parts, Body::from(body_bytes)))
}

// ------------------------------------------------------------------------------------------
// process_version_list, process_version_get, process_version_retire

async fn version_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(process): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    const OP: &str = "process_version_list";
    let result: Handled = async {
        let rid = &site.request_id;
        let call = begin(OP, &state, &site, &headers, &process)?;
        let (cursor, limit) = parse_query(OP, raw_query.as_deref(), true, rid)?;
        let page = call
            .port
            .list_versions(&call.ctx, &process, cursor.as_deref(), limit)
            .await;
        let page = page.map_err(|e| port_failure(OP, e, Code::ProcessNotFound, rid))?;
        let cursor_ok = page
            .next_cursor
            .as_ref()
            .is_none_or(|c| (1..=MAX_CURSOR_LEN).contains(&c.len()));
        if page.items.len() > limit as usize || !cursor_ok {
            return Err(violation(OP, rid));
        }
        let mut data = Vec::with_capacity(page.items.len());
        for version in &page.items {
            data.push(Json::Object(
                version_fields(version).ok_or_else(|| violation(OP, rid))?,
            ));
        }
        Ok(json_response(
            StatusCode::OK,
            json!({ "data": data, "next_cursor": page.next_cursor }),
        ))
    }
    .await;
    done(result)
}

async fn version_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((process, n)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    const OP: &str = "process_version_get";
    let result: Handled = async {
        let rid = &site.request_id;
        let call = begin(OP, &state, &site, &headers, &process)?;
        let version = version_of(OP, &n, rid)?;
        parse_query(OP, raw_query.as_deref(), false, rid)?;
        let detail = call.port.get_version(&call.ctx, &process, version).await;
        let detail = detail.map_err(|e| port_failure(OP, e, Code::ProcessNotFound, rid))?;
        let etag = etag_value(&detail.hash).ok_or_else(|| violation(OP, rid))?;
        let body = detail_json(&detail).ok_or_else(|| violation(OP, rid))?;
        // `If-None-Match` is honoured only after the port allowed the read, so a `304` never
        // tells a caller without the permission that a version exists.
        let mut response = if matches_etag(&headers, etag.to_str().unwrap_or_default()) {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            json_response(StatusCode::OK, json!({ "data": body }))
        };
        response.headers_mut().insert(header::ETAG, etag);
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        Ok(response)
    }
    .await;
    done(result)
}

async fn version_retire(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((process, n)): Path<(String, String)>,
    request: Request,
) -> Response {
    const OP: &str = "process_version_retire";
    let result: Handled = async {
        let rid = &site.request_id;
        let call = begin(OP, &state, &site, &headers, &process)?;
        let version = version_of(OP, &n, rid)?;
        // The operation takes no body: one that might name an actor is refused unread.
        if !read_body(OP, request, 1024, rid).await?.is_empty() {
            return Err(invalid(OP, "This call takes no body.", rid));
        }
        let retired = call.port.retire(&call.ctx, &process, version).await;
        let retired = retired.map_err(|e| port_failure(OP, e, Code::ProcessNotFound, rid))?;
        let fields = version_fields(&retired).ok_or_else(|| violation(OP, rid))?;
        Ok(json_response(
            StatusCode::OK,
            json!({ "data": Json::Object(fields) }),
        ))
    }
    .await;
    done(result)
}

// ------------------------------------------------------------------------------------------
// process_version_publish

async fn version_publish(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(process): Path<String>,
    request: Request,
) -> Response {
    const OP: &str = "process_version_publish";
    let result: Handled = async {
        let rid = &site.request_id;
        let mut call = begin(OP, &state, &site, &headers, &process)?;
        let key = idempotency_key(OP, &headers, rid)?;
        let bytes = read_body(OP, request, PUBLISH_BODY_CAP, rid).await?;
        let publish = publish_body(OP, &bytes, rid)?;
        call.ctx.idempotency_key = Some(key.clone());
        let path = format!("/api/v1/process/{process}/versions");
        let command = async {
            let published = call.port.publish(&call.ctx, &process, publish).await;
            let published =
                published.map_err(|e| port_failure(OP, e, Code::ProcessNotFound, rid))?;
            let fields = version_fields(&published.version).ok_or_else(|| violation(OP, rid))?;
            let etag = etag_value(&published.hash).ok_or_else(|| violation(OP, rid))?;
            let mut response =
                json_response(StatusCode::CREATED, json!({ "data": Json::Object(fields) }));
            response.headers_mut().insert(header::ETAG, etag);
            Ok::<Response, Stop>(response)
        };
        let keys = key_parts(&state, &site, &call, key, &path, &bytes)?;
        keyed(OP, keys, StatusCode::CREATED, rid, command).await
    }
    .await;
    done(result)
}

// ------------------------------------------------------------------------------------------
// process_migration_plan, process_migration_start, process_migration_get

/// A read-only `POST`: the plan changes nothing, takes no `Idempotency-Key` and stores nothing.
async fn migration_plan(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((process, from)): Path<(String, String)>,
    request: Request,
) -> Response {
    const OP: &str = "process_migration_plan";
    let result: Handled = async {
        let rid = &site.request_id;
        let call = begin(OP, &state, &site, &headers, &process)?;
        let from = version_of(OP, &from, rid)?;
        let bytes = read_body(OP, request, MIGRATION_BODY_CAP, rid).await?;
        let body = migration_body(OP, &bytes, rid)?;
        let plan = call
            .port
            .plan_migration(&call.ctx, &process, from, body)
            .await;
        let plan = plan.map_err(|e| port_failure(OP, e, Code::ProcessNotFound, rid))?;
        let data = plan_json(&plan).ok_or_else(|| violation(OP, rid))?;
        Ok(json_response(StatusCode::OK, json!({ "data": data })))
    }
    .await;
    done(result)
}

async fn migration_start(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((process, from)): Path<(String, String)>,
    request: Request,
) -> Response {
    const OP: &str = "process_migration_start";
    let result: Handled = async {
        let rid = &site.request_id;
        let mut call = begin(OP, &state, &site, &headers, &process)?;
        let from = version_of(OP, &from, rid)?;
        let key = idempotency_key(OP, &headers, rid)?;
        let bytes = read_body(OP, request, MIGRATION_BODY_CAP, rid).await?;
        let body = migration_body(OP, &bytes, rid)?;
        call.ctx.idempotency_key = Some(key.clone());
        let path = format!("/api/v1/process/{process}/versions/{from}/migrations");
        let command = async {
            let id = call
                .port
                .start_migration(&call.ctx, &process, from, body)
                .await;
            let id = id.map_err(|e| port_failure(OP, e, Code::ProcessNotFound, rid))?;
            if !is_opaque_id(&id) {
                return Err(violation(OP, rid));
            }
            Ok::<Response, Stop>(json_response(
                StatusCode::ACCEPTED,
                json!({ "migration_id": id }),
            ))
        };
        let keys = key_parts(&state, &site, &call, key, &path, &bytes)?;
        keyed(OP, keys, StatusCode::ACCEPTED, rid, command).await
    }
    .await;
    done(result)
}

async fn migration_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((process, id)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    const OP: &str = "process_migration_get";
    let result: Handled = async {
        let rid = &site.request_id;
        let call = begin(OP, &state, &site, &headers, &process)?;
        if !is_opaque_id(&id) {
            return Err(invalid(
                OP,
                "A migration id is not in the documented form.",
                rid,
            ));
        }
        parse_query(OP, raw_query.as_deref(), false, rid)?;
        let progress = call.port.get_migration(&call.ctx, &process, &id).await;
        let progress =
            progress.map_err(|e| port_failure(OP, e, Code::ProcessMigrationNotFound, rid))?;
        let data = progress_json(&progress).ok_or_else(|| violation(OP, rid))?;
        Ok(json_response(StatusCode::OK, json!({ "data": data })))
    }
    .await;
    done(result)
}
