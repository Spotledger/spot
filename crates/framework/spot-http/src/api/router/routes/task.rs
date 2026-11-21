//! The task inbox of native API 0.11.0 (WP-BPM-05.T7): `task_list`, `task_get`, `task_claim`,
//! `task_release`, `task_complete`, `task_delegate` and `task_reassign`.
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request and refuse a malformed one before the port is asked:
//! the path id, the closed query and body objects, the `If-Match` and `Idempotency-Key` headers
//! and every bound the contract names. They never decide who the caller is, what the caller may
//! see or do, or whether a task exists: the [`TaskPort`] answers for the caller its credential
//! names on the site the request resolved to, and the routes pass it no user, no role and no
//! tenant. A body, header or query parameter naming an actor is an unknown property and refused.
//!
//! * **Fail closed.** With no port every operation answers `501 service.not_configured`
//!   (never an empty `200`); a port failure that is not a declared refusal is `500 http.internal`
//!   with no detail.
//! * **No existence oracle.** An unknown task and one the caller may neither act on nor
//!   supervise are the port's one answer, `404 task.not_found`; the routes add nothing that
//!   tells them apart. Port errors carry no text: every answer uses the registry's message.
//! * **Logging.** A refused request is logged with the operation id and the problem code only;
//!   no body, token, comment or form value is logged.
//!
//! Every response is `private, no-store`.
#![allow(clippy::result_large_err)] // a refusal is a whole response, built once and returned

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, HttpBody as _};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value as Json, json};
use spot_storage_contract::Version;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::RouteState;
use crate::api::ports::PortKey;
use crate::api::ports::task::{
    CompleteIn, DelegateIn, ReassignIn, TaskDetailOut, TaskListIn, TaskListView, TaskPort,
    TaskPortError, TaskView,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{REPLAYED_HEADER, build_ctx, parse_if_match, render_error, take_body};

/// The operation ids this group serves.
pub const SERVES: &[&str] = &[
    "task_list",
    "task_get",
    "task_claim",
    "task_release",
    "task_complete",
    "task_delegate",
    "task_reassign",
];

/// The ports this group needs: the task port, whose adapter decides identity, visibility,
/// segregation, the version check and the audit. A group without it is not mounted.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn TaskPort>()];
    &KEYS
}

/// The task routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/task", get(task_list))
        .route("/api/v1/task/{id}", get(task_get))
        .route("/api/v1/task/{id}/claim", post(task_claim))
        .route("/api/v1/task/{id}/release", post(task_release))
        .route("/api/v1/task/{id}/complete", post(task_complete))
        .route("/api/v1/task/{id}/delegate", post(task_delegate))
        .route("/api/v1/task/{id}/reassign", post(task_reassign))
}

/// The contract's `limit` default.
const DEFAULT_LIMIT: usize = 50;
/// The contract's `limit` maximum.
const MAX_LIMIT: usize = 100;
/// The contract's `cursor` maximum length.
const MAX_CURSOR: usize = 2048;
/// The contract's `values` bound, serialised.
const MAX_VALUES_BYTES: usize = 64 * 1024;
/// The contract's `values` bound on properties (of the object and of every nested object).
const MAX_PROPERTIES: usize = 100;
/// How deep `values` may nest (the object itself is depth 1).
const MAX_VALUES_DEPTH: usize = 8;
/// The most items an array inside `values` may hold.
const MAX_VALUES_ITEMS: usize = 1000;
/// The most JSON nodes `values` may hold in all.
const MAX_VALUES_NODES: usize = 10_000;
/// The largest body any of the routes reads (the 64 KiB of `values` plus the other members).
const MAX_BODY_BYTES: usize = 72 * 1024;

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

/// A rendered refusal, logged by operation id and problem code only.
fn refuse(operation: &'static str, error: ApiError, request_id: &RequestId) -> Response {
    tracing::info!(
        operation,
        code = error.code.as_str(),
        "task request refused"
    );
    render_error(error, request_id)
}

fn private_no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no task service configured.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// The error for a port failure: only the code is honoured, never a text.
fn port_error(error: TaskPortError) -> ApiError {
    match error {
        TaskPortError::NotFound => ApiError::from_code(Code::TaskNotFound),
        TaskPortError::AlreadyClaimed => ApiError::from_code(Code::TaskAlreadyClaimed),
        TaskPortError::NotClaimedByCaller => ApiError::from_code(Code::TaskNotClaimedByCaller),
        TaskPortError::OutcomeInvalid => ApiError::from_code(Code::TaskOutcomeInvalid),
        TaskPortError::Segregation => ApiError::from_code(Code::TaskSegregation),
        TaskPortError::DelegationInvalid => ApiError::from_code(Code::TaskDelegationInvalid),
        TaskPortError::NotActive => ApiError::from_code(Code::TaskNotActive),
        TaskPortError::TargetInvalid => ApiError::from_code(Code::TaskTargetInvalid),
        TaskPortError::StaleVersion { current } => ApiError::from_code(Code::DocumentStaleVersion)
            .detail("current_version", Json::from(current)),
        TaskPortError::PermissionDenied => ApiError::from_code(Code::PermissionDenied),
        TaskPortError::IdempotencyConflict => ApiError::from_code(Code::ApiIdempotencyConflict),
        TaskPortError::CursorInvalid => invalid("`cursor` is not one this inbox issued."),
        TaskPortError::Unauthenticated => ApiError::from_code(Code::AuthSessionExpired),
        TaskPortError::Unavailable => ApiError::from_code(Code::ServerUnavailable),
        TaskPortError::Internal => ApiError::from_code(Code::HttpInternal),
    }
}

/// The contract's `TaskId`: `^[a-z][a-z0-9_]{0,63}$`.
fn is_task_id(raw: &str) -> bool {
    let b = raw.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// The contract's `Identifier`: `^[a-z][a-z0-9_]{0,62}$`.
fn is_identifier(raw: &str) -> bool {
    let b = raw.as_bytes();
    (1..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// The contract's `RunId`: `^[A-Za-z0-9_-]{16,64}$`.
fn is_run_id(raw: &str) -> bool {
    (16..=64).contains(&raw.len())
        && raw
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// The contract's `Idempotency-Key`: 1 to 128 visible ASCII characters.
fn is_idempotency_key(raw: &str) -> bool {
    (1..=128).contains(&raw.len()) && raw.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// A user reference: 1 to 128 characters, no control character.
fn is_user_ref(raw: &str) -> bool {
    (1..=128).contains(&raw.chars().count()) && !raw.chars().any(char::is_control)
}

/// Free text: at most `max` characters, no control character other than a line break or tab.
fn is_text(raw: &str, max: usize) -> bool {
    raw.chars().count() <= max
        && !raw
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The four query parameters of `task_list`, each at most once, nothing else.
fn parse_query(query: &str) -> Result<TaskListIn, ApiError> {
    let mut view: Option<TaskListView> = None;
    let mut process: Option<String> = None;
    let mut cursor: Option<String> = None;
    let mut limit: Option<usize> = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "view" => {
                if view.is_some() {
                    return Err(invalid("`view` can be given only once."));
                }
                view = Some(match value {
                    "mine" => TaskListView::Mine,
                    "team" => TaskListView::Team,
                    "overdue" => TaskListView::Overdue,
                    _ => return Err(invalid("`view` must be mine, team or overdue.")),
                });
            }
            "process" => {
                if process.is_some() {
                    return Err(invalid("`process` can be given only once."));
                }
                if !is_identifier(value) {
                    return Err(invalid("`process` must be a process name."));
                }
                process = Some(value.to_owned());
            }
            "cursor" => {
                if cursor.is_some() {
                    return Err(invalid("`cursor` can be given only once."));
                }
                let decoded = percent_decode(value)
                    .filter(|v| (1..=MAX_CURSOR).contains(&v.len()))
                    .filter(|v| v.bytes().all(|b| (0x21..=0x7e).contains(&b)))
                    .ok_or_else(|| invalid("`cursor` must be a cursor this inbox issued."))?;
                cursor = Some(decoded);
            }
            "limit" => {
                if limit.is_some() {
                    return Err(invalid("`limit` can be given only once."));
                }
                let n = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| value.parse::<usize>().ok())
                    .flatten()
                    .filter(|n| (1..=MAX_LIMIT).contains(n))
                    .ok_or_else(|| invalid("`limit` must be a whole number from 1 to 100."))?;
                limit = Some(n);
            }
            _ => {
                return Err(invalid(
                    "`view`, `process`, `cursor` and `limit` are the only query parameters.",
                ));
            }
        }
    }
    Ok(TaskListIn {
        view: view.unwrap_or(TaskListView::Mine),
        process,
        cursor,
        limit: limit.unwrap_or(DEFAULT_LIMIT),
    })
}

// ---------------------------------------------------------------------------------------------
// Response shapes

/// The contract's `TaskItem`, or `None` when a port item breaks the contract's bounds.
fn item_json(task: &TaskView) -> Option<Json> {
    let utc = |s: &str| s.ends_with('Z') && s.len() <= 40;
    let ok = is_task_id(&task.id)
        && is_run_id(&task.run)
        && is_identifier(&task.process)
        && (1..=128).contains(&task.node.chars().count())
        && (1..=280).contains(&task.label.chars().count())
        && task.due_at.as_deref().is_none_or(utc)
        && task.assignees.len() <= 100
        && task.assignees.iter().all(|a| is_user_ref(a))
        && task.claimed_by.as_deref().is_none_or(is_user_ref)
        && task.version >= 1
        && task
            .document
            .as_ref()
            .is_none_or(|(doctype, id)| is_identifier(doctype) && !id.is_empty() && id.len() <= 36);
    if !ok {
        return None;
    }
    let mut map = Map::new();
    map.insert("id".to_owned(), Json::from(task.id.clone()));
    map.insert("run".to_owned(), Json::from(task.run.clone()));
    map.insert("process".to_owned(), Json::from(task.process.clone()));
    map.insert("node".to_owned(), Json::from(task.node.clone()));
    map.insert("label".to_owned(), Json::from(task.label.clone()));
    map.insert("state".to_owned(), Json::from(task.state.as_str()));
    if let Some(due) = &task.due_at {
        map.insert("due_at".to_owned(), Json::from(due.clone()));
    }
    map.insert("assignees".to_owned(), json!(task.assignees));
    if let Some(holder) = &task.claimed_by {
        map.insert("claimed_by".to_owned(), Json::from(holder.clone()));
    }
    if let Some((doctype, id)) = &task.document {
        map.insert(
            "document".to_owned(),
            json!({ "doctype": doctype, "id": id }),
        );
    }
    map.insert("version".to_owned(), Json::from(task.version));
    Some(Json::Object(map))
}

/// The contract's `TaskDetail`, or `None` when the port's answer breaks the contract's bounds.
fn detail_json(detail: &TaskDetailOut) -> Option<Json> {
    let task = item_json(&detail.task)?;
    if detail.outcomes.len() > 20 || detail.fields.len() > 100 || detail.events.len() > 200 {
        return None;
    }
    let mut outcomes = Vec::with_capacity(detail.outcomes.len());
    for o in &detail.outcomes {
        if !(1..=128).contains(&o.outcome.chars().count())
            || !(1..=140).contains(&o.label.chars().count())
        {
            return None;
        }
        outcomes.push(json!({ "outcome": o.outcome, "label": o.label }));
    }
    let mut fields = Vec::with_capacity(detail.fields.len());
    for f in &detail.fields {
        if !is_identifier(&f.name) || f.field_type.is_empty() || f.field_type.len() > 32 {
            return None;
        }
        fields.push(json!({ "name": f.name, "type": f.field_type }));
    }
    let mut events = Vec::with_capacity(detail.events.len());
    for e in &detail.events {
        if !e.at.ends_with('Z')
            || e.at.len() > 40
            || !(1..=128).contains(&e.actor.chars().count())
            || e.on_behalf_of.as_deref().is_some_and(|u| !is_user_ref(u))
            || e.to.as_deref().is_some_and(|u| !is_user_ref(u))
            || e.outcome
                .as_deref()
                .is_some_and(|o| !(1..=128).contains(&o.chars().count()))
        {
            return None;
        }
        let mut map = Map::new();
        map.insert("at".to_owned(), Json::from(e.at.clone()));
        map.insert("kind".to_owned(), Json::from(e.kind.as_str()));
        map.insert("actor".to_owned(), Json::from(e.actor.clone()));
        if let Some(v) = &e.on_behalf_of {
            map.insert("on_behalf_of".to_owned(), Json::from(v.clone()));
        }
        if let Some(v) = &e.to {
            map.insert("to".to_owned(), Json::from(v.clone()));
        }
        if let Some(v) = &e.outcome {
            map.insert("outcome".to_owned(), Json::from(v.clone()));
        }
        events.push(Json::Object(map));
    }
    Some(json!({
        "task": task,
        "outcomes": outcomes,
        "fields": fields,
        "events": events,
    }))
}

/// A `200` with the task as `{data}` and its `ETag`.
fn task_response(task: &TaskView, request_id: &RequestId, operation: &'static str) -> Response {
    let Some(item) = item_json(task) else {
        tracing::error!(
            operation,
            "the task port returned a task outside the contract"
        );
        return refuse(
            operation,
            ApiError::from_code(Code::HttpInternal),
            request_id,
        );
    };
    let mut response = (StatusCode::OK, axum::Json(json!({ "data": item }))).into_response();
    stamp_etag(&mut response, task.version);
    response
}

fn stamp_etag(response: &mut Response, version: u64) {
    if let Ok(value) = HeaderValue::from_str(&format!("\"{version}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
}

// ---------------------------------------------------------------------------------------------
// Request shapes

/// The port and the request context of an authenticated call, or the answer to give without.
struct Call {
    port: Arc<dyn TaskPort>,
    ctx: RequestCtx,
}

fn call(
    state: &RouteState,
    site: &SiteContext,
    headers: &HeaderMap,
    operation: &'static str,
    key: Option<String>,
) -> Result<Call, Response> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn TaskPort>() else {
        return Err(not_configured(request_id));
    };
    let ctx = build_ctx(site, headers, key);
    if ctx.credential.is_none() {
        return Err(refuse(
            operation,
            ApiError::from_code(Code::AuthRequired),
            request_id,
        ));
    }
    Ok(Call { port, ctx })
}

/// A path id that is the contract's form, or the refusal.
fn path_id(raw: &str, operation: &'static str, request_id: &RequestId) -> Result<(), Response> {
    if is_task_id(raw) {
        Ok(())
    } else {
        Err(refuse(
            operation,
            invalid("A task id must be a task id of this API."),
            request_id,
        ))
    }
}

/// The required `If-Match` version.
fn if_match(
    headers: &HeaderMap,
    operation: &'static str,
    request_id: &RequestId,
) -> Result<u64, Response> {
    match parse_if_match(headers, true, request_id) {
        Ok(Some(Version(n))) => Ok(n),
        Ok(None) => Err(refuse(
            operation,
            ApiError::from_code(Code::ApiPreconditionRequired),
            request_id,
        )),
        Err(failure) => Err(private_no_store(failure.into_response())),
    }
}

/// The required `Idempotency-Key`.
fn idempotency_key(
    headers: &HeaderMap,
    operation: &'static str,
    request_id: &RequestId,
) -> Result<String, Response> {
    let mut values = headers.get_all("idempotency-key").iter();
    let first = values.next();
    let valid = match (first, values.next()) {
        (Some(value), None) => value.to_str().ok().filter(|v| is_idempotency_key(v)),
        _ => None,
    };
    valid.map(str::to_owned).ok_or_else(|| {
        refuse(
            operation,
            invalid("`Idempotency-Key` is required: 1 to 128 visible ASCII characters."),
            request_id,
        )
    })
}

/// The JSON object body, with a size check before anything is buffered.
async fn object_body(
    request: Request,
    operation: &'static str,
    request_id: &RequestId,
) -> Result<Map<String, Json>, Response> {
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    let hinted = request.body().size_hint().upper();
    if declared.is_some_and(|n| n > MAX_BODY_BYTES)
        || hinted.is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err(refuse(
            operation,
            ApiError::from_code(Code::HttpBodyTooLarge),
            request_id,
        ));
    }
    let body = match take_body(request, true, request_id).await {
        Ok(Some(body)) => body,
        Ok(None) => {
            return Err(refuse(
                operation,
                ApiError::from_code(Code::ApiInvalidJson),
                request_id,
            ));
        }
        Err(failure) => return Err(private_no_store(failure.into_response())),
    };
    match body {
        Json::Object(map) => Ok(map),
        _ => Err(refuse(
            operation,
            invalid("The request body must be a JSON object."),
            request_id,
        )),
    }
}

/// Refuses a member the contract's closed object does not have. The name is not echoed.
fn closed(map: &Map<String, Json>, allowed: &[&str]) -> Result<(), ApiError> {
    if map.keys().all(|k| allowed.contains(&k.as_str())) {
        Ok(())
    } else {
        Err(invalid(
            "The body has a member this operation does not take; the actor is always the caller.",
        ))
    }
}

fn required_string(map: &Map<String, Json>, name: &str) -> Result<String, ApiError> {
    match map.get(name) {
        Some(Json::String(s)) => Ok(s.clone()),
        _ => Err(invalid(&format!(
            "`{name}` is required and must be a string."
        ))),
    }
}

fn optional_text(
    map: &Map<String, Json>,
    name: &str,
    max: usize,
) -> Result<Option<String>, ApiError> {
    match map.get(name) {
        None => Ok(None),
        Some(Json::String(s)) if is_text(s, max) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(&format!(
            "`{name}` must be text of at most {max} characters."
        ))),
    }
}

fn user_ref(map: &Map<String, Json>, name: &str) -> Result<String, ApiError> {
    let value = required_string(map, name)?;
    if is_user_ref(&value) {
        Ok(value)
    } else {
        Err(invalid(&format!("`{name}` must be a user id.")))
    }
}

/// Counts a JSON value against the bounds; `false` when any is broken.
fn bounded(value: &Json, depth: usize, nodes: &mut usize) -> bool {
    *nodes += 1;
    if *nodes > MAX_VALUES_NODES || depth > MAX_VALUES_DEPTH {
        return false;
    }
    match value {
        Json::Object(map) => {
            map.len() <= MAX_PROPERTIES
                && map.keys().all(|k| (1..=128).contains(&k.chars().count()))
                && map.values().all(|v| bounded(v, depth + 1, nodes))
        }
        Json::Array(items) => {
            items.len() <= MAX_VALUES_ITEMS && items.iter().all(|v| bounded(v, depth + 1, nodes))
        }
        _ => true,
    }
}

fn complete_of(map: &Map<String, Json>) -> Result<CompleteIn, ApiError> {
    closed(map, &["outcome", "comment", "values"])?;
    let outcome = required_string(map, "outcome")?;
    if !(1..=128).contains(&outcome.chars().count()) || outcome.chars().any(char::is_control) {
        return Err(invalid("`outcome` must be 1 to 128 characters."));
    }
    let comment = optional_text(map, "comment", 2000)?;
    let values = match map.get("values") {
        None => None,
        Some(Json::Object(values)) => {
            let whole = Json::Object(values.clone());
            let mut nodes = 0;
            let size_ok = serde_json::to_vec(&whole).is_ok_and(|v| v.len() <= MAX_VALUES_BYTES);
            let names_ok = values.keys().all(|k| is_identifier(k));
            if !size_ok || !names_ok || !bounded(&whole, 1, &mut nodes) {
                return Err(invalid(
                    "`values` is bounded: field names, at most 100 properties, 64 KiB, shallow.",
                ));
            }
            Some(values.clone())
        }
        Some(_) => return Err(invalid("`values` must be an object.")),
    };
    Ok(CompleteIn {
        outcome,
        comment,
        values,
    })
}

fn delegate_of(map: &Map<String, Json>) -> Result<DelegateIn, ApiError> {
    closed(map, &["to", "until", "reason"])?;
    let to = user_ref(map, "to")?;
    let until = match map.get("until") {
        None => None,
        Some(Json::String(s)) if s.len() <= 40 => Some(
            OffsetDateTime::parse(s, &Rfc3339)
                .map_err(|_| invalid("`until` must be an RFC 3339 instant."))?,
        ),
        Some(_) => return Err(invalid("`until` must be an RFC 3339 instant.")),
    };
    let reason = optional_text(map, "reason", 500)?;
    Ok(DelegateIn { to, until, reason })
}

fn reassign_of(map: &Map<String, Json>) -> Result<ReassignIn, ApiError> {
    closed(map, &["to", "reason"])?;
    Ok(ReassignIn {
        to: user_ref(map, "to")?,
        reason: optional_text(map, "reason", 500)?,
    })
}

// ---------------------------------------------------------------------------------------------
// Handlers

async fn task_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    const OP: &str = "task_list";
    let request_id = &site.request_id;
    // Read before the block so the future holds no reference to the (non-`Sync`) request.
    let query_text = request.uri().query().unwrap_or("").to_owned();
    drop(request);
    let response = async {
        let call = call(&state, &site, &headers, OP, None)?;
        let query = parse_query(&query_text).map_err(|e| refuse(OP, e, request_id))?;
        let page = call
            .port
            .list(&call.ctx, &query)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        let internal = || refuse(OP, ApiError::from_code(Code::HttpInternal), request_id);
        if page.tasks.len() > query.limit {
            tracing::error!(operation = OP, "the task port returned more than the limit");
            return Err(internal());
        }
        let mut data = Vec::with_capacity(page.tasks.len());
        for task in &page.tasks {
            match item_json(task) {
                Some(item) => data.push(item),
                None => {
                    tracing::error!(
                        operation = OP,
                        "the task port returned a task outside the contract"
                    );
                    return Err(internal());
                }
            }
        }
        if page
            .next_cursor
            .as_deref()
            .is_some_and(|c| !(1..=MAX_CURSOR).contains(&c.len()))
        {
            return Err(internal());
        }
        Ok::<Response, Response>(
            (
                StatusCode::OK,
                axum::Json(json!({ "data": data, "next_cursor": page.next_cursor })),
            )
                .into_response(),
        )
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn task_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    const OP: &str = "task_get";
    let request_id = &site.request_id;
    let response = async {
        let call = call(&state, &site, &headers, OP, None)?;
        path_id(&id, OP, request_id)?;
        let detail = call
            .port
            .get(&call.ctx, &id)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        let Some(data) = detail_json(&detail) else {
            tracing::error!(
                operation = OP,
                "the task port returned a task outside the contract"
            );
            return Err(refuse(
                OP,
                ApiError::from_code(Code::HttpInternal),
                request_id,
            ));
        };
        let mut response = (StatusCode::OK, axum::Json(json!({ "data": data }))).into_response();
        stamp_etag(&mut response, detail.task.version);
        Ok::<Response, Response>(response)
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn task_claim(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    const OP: &str = "task_claim";
    let request_id = &site.request_id;
    let response = async {
        let call = call(&state, &site, &headers, OP, None)?;
        path_id(&id, OP, request_id)?;
        let expected = if_match(&headers, OP, request_id)?;
        let task = call
            .port
            .claim(&call.ctx, &id, expected)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        Ok::<Response, Response>(task_response(&task, request_id, OP))
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn task_release(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    const OP: &str = "task_release";
    let request_id = &site.request_id;
    let response = async {
        let call = call(&state, &site, &headers, OP, None)?;
        path_id(&id, OP, request_id)?;
        let expected = if_match(&headers, OP, request_id)?;
        let task = call
            .port
            .release(&call.ctx, &id, expected)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        Ok::<Response, Response>(task_response(&task, request_id, OP))
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn task_complete(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    const OP: &str = "task_complete";
    let request_id = &site.request_id;
    let response = async {
        // The key is validated before the context is built, so the port sees only a well
        // formed one. Authentication is still checked first inside `call`.
        let authed = call(&state, &site, &headers, OP, None)?;
        path_id(&id, OP, request_id)?;
        let expected = if_match(&headers, OP, request_id)?;
        let key = idempotency_key(&headers, OP, request_id)?;
        let map = object_body(request, OP, request_id).await?;
        let body = complete_of(&map).map_err(|e| refuse(OP, e, request_id))?;
        let mut ctx = authed.ctx;
        ctx.idempotency_key = Some(key);
        let done = authed
            .port
            .complete(&ctx, &id, expected, &body)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        let mut response = task_response(&done.task, request_id, OP);
        if done.replayed && response.status() == StatusCode::OK {
            response
                .headers_mut()
                .insert(REPLAYED_HEADER, HeaderValue::from_static("true"));
        }
        Ok::<Response, Response>(response)
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn task_delegate(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    const OP: &str = "task_delegate";
    let request_id = &site.request_id;
    let response = async {
        let call = call(&state, &site, &headers, OP, None)?;
        path_id(&id, OP, request_id)?;
        let expected = if_match(&headers, OP, request_id)?;
        let map = object_body(request, OP, request_id).await?;
        let body = delegate_of(&map).map_err(|e| refuse(OP, e, request_id))?;
        let task = call
            .port
            .delegate(&call.ctx, &id, expected, &body)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        Ok::<Response, Response>(task_response(&task, request_id, OP))
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn task_reassign(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    const OP: &str = "task_reassign";
    let request_id = &site.request_id;
    let response = async {
        let call = call(&state, &site, &headers, OP, None)?;
        path_id(&id, OP, request_id)?;
        let expected = if_match(&headers, OP, request_id)?;
        let map = object_body(request, OP, request_id).await?;
        let body = reassign_of(&map).map_err(|e| refuse(OP, e, request_id))?;
        let task = call
            .port
            .reassign(&call.ctx, &id, expected, &body)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        Ok::<Response, Response>(task_response(&task, request_id, OP))
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_shapes_are_the_contracts() {
        assert!(is_task_id("t_1"));
        assert!(is_task_id(&format!("a{}", "b".repeat(63))));
        assert!(!is_task_id(&format!("a{}", "b".repeat(64))));
        assert!(!is_task_id(""));
        assert!(!is_task_id("T1"));
        assert!(!is_task_id("1t"));
        assert!(!is_task_id("t-1"));
        assert!(!is_task_id("../x"));
        assert!(is_idempotency_key("abc-123"));
        assert!(!is_idempotency_key("has space"));
        assert!(!is_idempotency_key(&"k".repeat(129)));
    }

    #[test]
    fn the_query_is_closed_and_bounded() {
        assert!(parse_query("").is_ok());
        assert!(parse_query("view=team&limit=100").is_ok());
        for bad in [
            "view=all",
            "view=mine&view=team",
            "limit=0",
            "limit=101",
            "limit=x",
            "process=Bad",
            "user=alice",
            "actor=system%3Asla",
            "cursor=",
        ] {
            assert!(parse_query(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn values_are_bounded_in_depth_properties_and_bytes() {
        let ok = serde_json::json!({"outcome": "approve", "values": {"a": {"b": [1, 2]}}});
        let ok = ok.as_object().cloned().unwrap_or_default();
        assert!(complete_of(&ok).is_ok());
        let mut deep = serde_json::json!(1);
        for _ in 0..MAX_VALUES_DEPTH {
            deep = serde_json::json!({ "k": deep });
        }
        let map = serde_json::json!({"outcome": "approve", "values": {"a": deep}});
        assert!(complete_of(map.as_object().unwrap_or(&Map::new())).is_err());
        let many: Map<String, Json> = (0..101).map(|i| (format!("f{i}"), Json::from(i))).collect();
        let map = serde_json::json!({"outcome": "approve", "values": many});
        assert!(complete_of(map.as_object().unwrap_or(&Map::new())).is_err());
        let map = serde_json::json!({"outcome": "approve", "values": {"a": "x".repeat(70_000)}});
        assert!(complete_of(map.as_object().unwrap_or(&Map::new())).is_err());
        let map = serde_json::json!({"outcome": "approve", "actor": "root"});
        assert!(complete_of(map.as_object().unwrap_or(&Map::new())).is_err());
    }
}
