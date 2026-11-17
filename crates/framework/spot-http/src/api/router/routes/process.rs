//! The process and run operations (native API 0.12.0, WP-BPM-02.T5): `process_list`,
//! `run_start`, `run_list`, `run_get`, `run_events` and `run_cancel`. The seventh operation of
//! the group, the public `process_webhook`, is in `routes/webhook.rs`.
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the **shape** of a request (path ids, query parameters, the closed JSON body,
//! the `Idempotency-Key`) and refuse a malformed one before the port is asked. They never decide
//! who the caller is, which processes the caller may start, which runs the caller may see or
//! cancel: the [`ProcessPort`] answers for the caller its credential names, and the routes pass it
//! **no user, no role and no tenant**. The tenant is the site the request resolved to; no path,
//! header, query parameter or body field can name another. A body field, a query parameter or a
//! path segment that names an actor is refused as unknown (every body is closed and so is every
//! query); a header that names one is never read.
//!
//! # Fail closed
//!
//! - A group without its ports is not mounted at all (the registry). A host that mounts the
//!   routes itself over a state with no [`ProcessPort`] gets `501 service.not_configured` from
//!   every operation, never `200` with an empty list.
//! - A port error outside the declared refusals is `500 http.internal` with no detail.
//! - A port answer outside the contract (a page longer than asked, a run with too many steps)
//!   is `500 http.internal` too, and is logged without its content.
//! - The existence oracle is closed by the port: a process the caller may not start and one
//!   that does not exist are one error, and so are a run the caller may not see and one that
//!   does not exist; the route renders each error the same way, byte for byte but the request id.
//!
//! # `run_start` and the `Idempotency-Key`
//!
//! The key is required. The route keeps the key protocol itself, through the one
//! [`IdempotencyStore`] the host provides as a port (the registered groups sit outside the JSON
//! command middleware): the first request answers `201`, a replay of the same key and body
//! answers `200` with the first run and `Idempotency-Replayed: true`, and the same key with a
//! different body is `409 api.idempotency_conflict`. The scope is the site and the credential
//! (never across them); the fingerprint covers the method, the path (so the process name) and
//! the caller. Only a `201` is stored; every refusal releases the key so a corrected retry runs.
//! The key also reaches the port as `ctx.idempotency_key`, the start's source event.
//!
//! `ETag` and `If-Match` do not appear in the contract for these operations, so none is read or
//! written. Every response is `Cache-Control: private, no-store`.
//!
//! # Logging
//!
//! A refused request is logged with the operation id and the problem code only. No body, token,
//! input value, reason or key is logged.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value as Json, json};

use super::{Registration, RouteState};
use crate::api::idempotency::{IdempotencyStore, KeyScope, fingerprint};
use crate::api::ports::PortKey;
use crate::api::ports::process::{
    DEFAULT_EVENTS_PAGE, DEFAULT_PAGE, ListPage, MAX_DOCUMENTS, MAX_EVENTS_PAGE, MAX_PAGE,
    MAX_STEPS, ProcessPort, ProcessPortError, Run, RunEvent, RunQuery, RunStart, RunState,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, FieldIssue, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::middleware::{
    Admission, CLASS_HTTP_API, Outcome, Persist, ReservationGuard, admit, replay_response, settle,
};
use super::super::{Failure, build_ctx, credential_hash, fail};

/// The operation ids this group serves once registered.
pub const SERVES: &[&str] = &[
    "process_list",
    "run_start",
    "run_list",
    "run_get",
    "run_events",
    "run_cancel",
];

/// The ports this group needs: the process port (whose adapter decides identity, rights,
/// visibility and the audit) and the idempotency store `run_start` keeps its key in. A group
/// without either is not mounted at all.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 2] = [
        PortKey::of::<dyn ProcessPort>(),
        PortKey::of::<dyn IdempotencyStore>(),
    ];
    &KEYS
}

/// The process and run routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/process", get(process_list))
        .route("/api/v1/process/{process}/runs", post(run_start))
        .route("/api/v1/process/runs", get(run_list))
        .route("/api/v1/process/runs/{id}", get(run_get))
        .route("/api/v1/process/runs/{id}/events", get(run_events))
        .route("/api/v1/process/runs/{id}/cancel", post(run_cancel))
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "process",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

/// Largest `run_start` body read: the input is at most 64 KiB serialised, plus the envelope.
const RUN_START_BODY_CAP: usize = 128 * 1024;
/// Largest `run_cancel` body read: a reason of 500 characters is at most 2,000 bytes.
const RUN_CANCEL_BODY_CAP: usize = 16 * 1024;
/// The contract's bound on the serialised start input.
const MAX_INPUT_BYTES: usize = 64 * 1024;
/// The contract's bound on the start input's properties.
const MAX_INPUT_PROPERTIES: usize = 200;
/// The contract's bound on a reason.
const MAX_REASON_CHARS: usize = 500;
/// The contract's bound on a cursor.
const MAX_CURSOR_LEN: usize = 2048;
/// The contract's bound on a `business_key`.
const MAX_BUSINESS_KEY: usize = 128;
/// The most field issues one `process.input_invalid` carries.
const MAX_FIELD_ISSUES: usize = 100;
/// The longest field path one issue carries.
const MAX_FIELD_PATH: usize = 256;
/// The fixed message of every field issue: generic, never a submitted value.
const FIELD_MESSAGE: &str = "This field is not valid.";

// ------------------------------------------------------------------------------------------
// Answers

/// Why a handler stopped: an error to render, or a ready answer (`501`).
enum Stop {
    Fail(Failure),
    Respond(Box<Response>),
}

impl From<Failure> for Stop {
    fn from(failure: Failure) -> Stop {
        Stop::Fail(failure)
    }
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

/// Every response of the group is per caller and never cached.
fn done(result: Handled) -> Response {
    let mut response = match result {
        Ok(response) => response,
        Err(stop) => stop.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
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
    tracing::error!(operation, "the process port answered outside the contract");
    Stop::Fail(fail(ApiError::from_code(Code::HttpInternal), request_id))
}

/// `501 service.not_configured`: the site has no process service.
fn not_configured(request_id: &RequestId) -> Stop {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no process service configured.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Stop::Respond(Box::new(response))
}

/// A port refusal as the contract's error: only the declared codes, and no detail.
fn port_failure(operation: &'static str, error: ProcessPortError, request_id: &RequestId) -> Stop {
    let api_error = match error {
        ProcessPortError::NotFound => ApiError::from_code(Code::ProcessNotFound),
        ProcessPortError::InputInvalid(issues) => {
            let mut api_error = ApiError::from_code(Code::ProcessInputInvalid);
            for issue in issues.iter().take(MAX_FIELD_ISSUES) {
                if issue.field.is_empty() || issue.field.len() > MAX_FIELD_PATH {
                    continue;
                }
                // Field paths and stable codes only: the message is fixed, never the port's.
                api_error = api_error.with_field(FieldIssue::new(
                    issue.field.clone(),
                    issue.code.clone(),
                    FIELD_MESSAGE,
                ));
            }
            api_error
        }
        ProcessPortError::RunNotFound => ApiError::from_code(Code::ProcessRunNotFound),
        ProcessPortError::RunNotActive => ApiError::from_code(Code::ProcessRunNotActive),
        ProcessPortError::PermissionDenied => ApiError::from_code(Code::PermissionDenied),
        ProcessPortError::BadCursor => ApiError::new(
            Code::ApiInvalidRequest,
            "`cursor` is not one this service issued.",
        ),
        ProcessPortError::Unavailable => ApiError::from_code(Code::ServerUnavailable),
        ProcessPortError::Internal => {
            tracing::error!(operation, "the process port failed");
            ApiError::from_code(Code::HttpInternal)
        }
    };
    refuse(operation, api_error, request_id)
}

// ------------------------------------------------------------------------------------------
// The call and its parts

/// The port and the request context of an authenticated call.
struct Call {
    port: Arc<dyn ProcessPort>,
    ctx: RequestCtx,
}

fn call(
    operation: &'static str,
    state: &RouteState,
    site: &SiteContext,
    headers: &HeaderMap,
) -> Result<Call, Stop> {
    let request_id = &site.request_id;
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(refuse(
            operation,
            ApiError::from_code(Code::AuthRequired),
            request_id,
        ));
    }
    let Some(port) = state.port::<dyn ProcessPort>() else {
        return Err(not_configured(request_id));
    };
    Ok(Call { port, ctx })
}

fn is_identifier(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(|b| b.is_ascii_lowercase())
        && bytes
            .iter()
            .skip(1)
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

fn is_run_id(raw: &str) -> bool {
    (16..=64).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_visible_ascii(raw: &str) -> bool {
    raw.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decodes `%XX` escapes and `+`; `None` for a bad escape or a result that is not UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'%' => {
                let high = hex_value(*bytes.get(i + 1)?)?;
                let low = hex_value(*bytes.get(i + 2)?)?;
                out.push(high * 16 + low);
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

/// The query as a map: only the `allowed` keys, each at most once. Anything else is
/// `400 api.invalid_request`, so a parameter that names an actor, a tenant or a role is refused.
fn parse_query(
    operation: &'static str,
    raw: Option<&str>,
    allowed: &[&str],
    request_id: &RequestId,
) -> Result<HashMap<String, String>, Stop> {
    let mut map = HashMap::new();
    for pair in raw.unwrap_or_default().split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (Some(key), Some(value)) = (percent_decode(key), percent_decode(value)) else {
            return Err(invalid(operation, "The query is not valid.", request_id));
        };
        if !allowed.contains(&key.as_str()) {
            return Err(invalid(
                operation,
                "This call does not take that query parameter.",
                request_id,
            ));
        }
        if map.insert(key, value).is_some() {
            return Err(invalid(
                operation,
                "A query parameter can be given only once.",
                request_id,
            ));
        }
    }
    Ok(map)
}

/// A decimal number: digits only, at most nine of them.
fn small_number(raw: &str) -> Option<u32> {
    if raw.is_empty() || raw.len() > 9 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u32>().ok()
}

fn limit_of(
    operation: &'static str,
    query: &HashMap<String, String>,
    default: u32,
    max: u32,
    request_id: &RequestId,
) -> Result<u32, Stop> {
    match query.get("limit") {
        None => Ok(default),
        Some(raw) => match small_number(raw) {
            Some(n) if (1..=max).contains(&n) => Ok(n),
            _ => Err(invalid(
                operation,
                &format!("`limit` must be between 1 and {max}."),
                request_id,
            )),
        },
    }
}

fn cursor_of(
    operation: &'static str,
    query: &HashMap<String, String>,
    request_id: &RequestId,
) -> Result<Option<String>, Stop> {
    match query.get("cursor") {
        None => Ok(None),
        Some(raw) if (1..=MAX_CURSOR_LEN).contains(&raw.len()) && is_visible_ascii(raw) => {
            Ok(Some(raw.clone()))
        }
        Some(_) => Err(invalid(operation, "`cursor` is not valid.", request_id)),
    }
}

/// Reads a JSON body of at most `cap` bytes: the media type, the declared and the real size are
/// all checked, and the body is never buffered past `cap` (the read stops when it is exceeded).
async fn read_json(
    operation: &'static str,
    request: Request,
    cap: usize,
    request_id: &RequestId,
) -> Result<(Vec<u8>, Json), Stop> {
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    if let Some(content_type) = &content_type
        && content_type != "application/json"
    {
        return Err(refuse(
            operation,
            ApiError::from_code(Code::ApiUnsupportedMediaType),
            request_id,
        ));
    }
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok());
    if declared.is_some_and(|n| n > cap) {
        return Err(refuse(
            operation,
            ApiError::from_code(Code::HttpBodyTooLarge),
            request_id,
        ));
    }
    let bytes = axum::body::to_bytes(body, cap).await.map_err(|_| {
        refuse(
            operation,
            ApiError::from_code(Code::HttpBodyTooLarge),
            request_id,
        )
    })?;
    if bytes.is_empty() {
        return Err(refuse(
            operation,
            ApiError::new(
                Code::ApiInvalidJson,
                "The request body must be a JSON document.",
            ),
            request_id,
        ));
    }
    let parsed: Json = serde_json::from_slice(&bytes).map_err(|_| {
        refuse(
            operation,
            ApiError::from_code(Code::ApiInvalidJson),
            request_id,
        )
    })?;
    Ok((bytes.to_vec(), parsed))
}

/// The body as an object whose keys are all in `allowed` (the contract's bodies are closed).
fn closed_object(
    operation: &'static str,
    body: Json,
    allowed: &[&str],
    request_id: &RequestId,
) -> Result<Map<String, Json>, Stop> {
    let Json::Object(map) = body else {
        return Err(invalid(
            operation,
            "The request body must be a JSON object.",
            request_id,
        ));
    };
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid(
            operation,
            "The request body has a property this call does not take.",
            request_id,
        ));
    }
    Ok(map)
}

// ------------------------------------------------------------------------------------------
// JSON of the contract's schemas

fn some_string(map: &mut Map<String, Json>, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), Json::from(value.clone()));
    }
}

/// The contract's `Run`; `None` when the port's run is outside the contract's bounds.
fn run_json(run: &Run) -> Option<Json> {
    if run.steps.len() > MAX_STEPS || run.documents.len() > MAX_DOCUMENTS {
        return None;
    }
    let mut map = Map::new();
    map.insert("id".to_owned(), Json::from(run.id.clone()));
    map.insert("process".to_owned(), Json::from(run.process.clone()));
    map.insert("version".to_owned(), Json::from(run.version));
    map.insert("state".to_owned(), Json::from(run.state.as_str()));
    map.insert(
        "business_key".to_owned(),
        match &run.business_key {
            Some(key) => Json::from(key.clone()),
            None => Json::Null,
        },
    );
    map.insert("started_at".to_owned(), Json::from(run.started_at.clone()));
    some_string(&mut map, "ended_at", &run.ended_at);
    if run.state == RunState::Failed {
        some_string(&mut map, "failure_code", &run.failure_code);
    }
    let steps: Vec<Json> = run
        .steps
        .iter()
        .map(|step| {
            let mut item = Map::new();
            item.insert("node".to_owned(), Json::from(step.node.clone()));
            item.insert("label".to_owned(), Json::from(step.label.clone()));
            item.insert("state".to_owned(), Json::from(step.state.as_str()));
            some_string(&mut item, "started_at", &step.started_at);
            some_string(&mut item, "ended_at", &step.ended_at);
            Json::Object(item)
        })
        .collect();
    map.insert("steps".to_owned(), Json::Array(steps));
    let documents: Vec<Json> = run
        .documents
        .iter()
        .map(|d| json!({ "doctype": d.doctype, "id": d.id }))
        .collect();
    map.insert("documents".to_owned(), Json::Array(documents));
    Some(Json::Object(map))
}

fn event_json(event: &RunEvent) -> Json {
    let mut map = Map::new();
    map.insert("seq".to_owned(), Json::from(event.seq));
    map.insert("at".to_owned(), Json::from(event.at.clone()));
    map.insert("kind".to_owned(), Json::from(event.kind.clone()));
    some_string(&mut map, "node", &event.node);
    some_string(&mut map, "actor", &event.actor);
    Json::Object(map)
}

/// A page of the contract's `{data, next_cursor}`; `None` when the port's page is longer than
/// the caller asked for, or its cursor is outside the contract's bound.
fn cursor_page<T>(
    page: &ListPage<T>,
    limit: u32,
    item: impl Fn(&T) -> Option<Json>,
) -> Option<Json> {
    if page.items.len() > limit as usize
        || page
            .next_cursor
            .as_ref()
            .is_some_and(|c| c.is_empty() || c.len() > MAX_CURSOR_LEN)
    {
        return None;
    }
    let mut data = Vec::with_capacity(page.items.len());
    for entry in &page.items {
        data.push(item(entry)?);
    }
    Some(json!({ "data": data, "next_cursor": page.next_cursor }))
}

fn ok_json(body: Json) -> Response {
    (StatusCode::OK, axum::Json(body)).into_response()
}

// ------------------------------------------------------------------------------------------
// process_list

async fn process_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    const OP: &str = "process_list";
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(OP, &state, &site, &headers)?;
        let query = parse_query(OP, raw_query.as_deref(), &["cursor", "limit"], request_id)?;
        let limit = limit_of(OP, &query, DEFAULT_PAGE, MAX_PAGE, request_id)?;
        let cursor = cursor_of(OP, &query, request_id)?;
        let page = call
            .port
            .list_processes(&call.ctx, cursor.as_deref(), limit)
            .await
            .map_err(|e| port_failure(OP, e, request_id))?;
        let body = cursor_page(&page, limit, |summary| {
            Some(json!({
                "name": summary.name,
                "label": summary.label,
                "version": summary.version,
                "input_schema": summary.input_schema,
            }))
        })
        .ok_or_else(|| violation(OP, request_id))?;
        Ok(ok_json(body))
    }
    .await;
    done(result)
}

// ------------------------------------------------------------------------------------------
// run_start

/// The `Idempotency-Key`: required, exactly once, 1 to 128 visible ASCII characters.
fn idempotency_key(headers: &HeaderMap, request_id: &RequestId) -> Result<String, Stop> {
    const OP: &str = "run_start";
    let mut values = headers.get_all("idempotency-key").iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err(invalid(
            OP,
            "An Idempotency-Key header is required, exactly once.",
            request_id,
        ));
    };
    match value.to_str() {
        Ok(key) if (1..=128).contains(&key.len()) && is_visible_ascii(key) => Ok(key.to_owned()),
        _ => Err(invalid(
            OP,
            "The Idempotency-Key header must be 1 to 128 visible ASCII characters.",
            request_id,
        )),
    }
}

/// The closed `RunStart` body, bounded as the contract says.
fn run_start_body(body: Json, request_id: &RequestId) -> Result<RunStart, Stop> {
    const OP: &str = "run_start";
    let mut map = closed_object(OP, body, &["input", "business_key"], request_id)?;
    let Some(Json::Object(input)) = map.remove("input") else {
        return Err(invalid(
            OP,
            "`input` is required and must be an object.",
            request_id,
        ));
    };
    let fits = serde_json::to_vec(&input).is_ok_and(|bytes| bytes.len() <= MAX_INPUT_BYTES);
    let too_big = input.len() > MAX_INPUT_PROPERTIES || !fits;
    if too_big {
        return Err(invalid(
            OP,
            "`input` is larger than a start takes.",
            request_id,
        ));
    }
    let business_key = match map.remove("business_key") {
        None => None,
        Some(Json::String(key))
            if (1..=MAX_BUSINESS_KEY).contains(&key.len()) && is_visible_ascii(&key) =>
        {
            Some(key)
        }
        Some(_) => {
            return Err(invalid(
                OP,
                "`business_key` must be 1 to 128 visible ASCII characters.",
                request_id,
            ));
        }
    };
    Ok(RunStart {
        input,
        business_key,
    })
}

async fn run_start(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(process): Path<String>,
    request: Request,
) -> Response {
    const OP: &str = "run_start";
    let result: Handled = async {
        let request_id = &site.request_id;
        let mut call = call(OP, &state, &site, &headers)?;
        let Some(store) = state.port::<dyn IdempotencyStore>() else {
            // Never start a run without the key's reservation.
            return Err(not_configured(request_id));
        };
        if !is_identifier(&process) {
            return Err(invalid(
                OP,
                "A process name must be a storage name.",
                request_id,
            ));
        }
        let key = idempotency_key(&headers, request_id)?;
        let (bytes, parsed) = read_json(OP, request, RUN_START_BODY_CAP, request_id).await?;
        let start = run_start_body(parsed, request_id)?;
        call.ctx.idempotency_key = Some(key.clone());
        let credential = credential_hash(call.ctx.credential.as_ref());
        let scope = KeyScope {
            site: site.site.as_str().to_owned(),
            credential: credential.clone(),
            key,
        };
        // The process name (in the path) and the caller are part of what the key vouches for.
        let print = fingerprint(
            "POST",
            &format!("/api/v1/process/{process}/runs\n{credential}"),
            &bytes,
        );
        match admit(&*store, &scope, &print).await {
            Err(error) => return Err(refuse(OP, error, request_id)),
            Ok(Admission::Conflict) => {
                return Err(refuse(
                    OP,
                    ApiError::from_code(Code::ApiIdempotencyConflict),
                    request_id,
                ));
            }
            Ok(Admission::Replay(stored)) => {
                // The contract's replay is `200`, whatever the first answer's status was.
                let mut response = replay_response(&stored, request_id);
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
        let started: Handled = async {
            let run = call
                .port
                .start_run(&call.ctx, &process, start)
                .await
                .map_err(|e| port_failure(OP, e, request_id))?;
            let data = run_json(&run).ok_or_else(|| violation(OP, request_id))?;
            Ok((StatusCode::CREATED, axum::Json(json!({ "data": data }))).into_response())
        }
        .await;
        let response = match started {
            Ok(response) => response,
            Err(stop) => stop.into_response(),
        };
        let status = response.status();
        let (parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default();
        // Only the `201` is kept; every refusal releases the key so a corrected retry runs.
        settle(
            &*store,
            &scope,
            &print,
            Outcome {
                persist: Persist::Body,
                class: CLASS_HTTP_API,
                release: status != StatusCode::CREATED,
            },
            &parts,
            &body_bytes,
        )
        .await;
        guard.armed = false;
        Ok(Response::from_parts(parts, Body::from(body_bytes)))
    }
    .await;
    done(result)
}

// ------------------------------------------------------------------------------------------
// run_list

async fn run_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    const OP: &str = "run_list";
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(OP, &state, &site, &headers)?;
        let query = parse_query(
            OP,
            raw_query.as_deref(),
            &["process", "state", "mine", "cursor", "limit"],
            request_id,
        )?;
        let limit = limit_of(OP, &query, DEFAULT_PAGE, MAX_PAGE, request_id)?;
        let cursor = cursor_of(OP, &query, request_id)?;
        let process = match query.get("process") {
            None => None,
            Some(raw) if is_identifier(raw) => Some(raw.clone()),
            Some(_) => {
                return Err(invalid(OP, "`process` must be a process name.", request_id));
            }
        };
        let state_filter = match query.get("state") {
            None => None,
            Some(raw) => match RunState::parse(raw) {
                Some(parsed) => Some(parsed),
                None => return Err(invalid(OP, "`state` is not a run state.", request_id)),
            },
        };
        let mine = match query.get("mine").map(String::as_str) {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid(OP, "`mine` must be true or false.", request_id)),
        };
        let page = call
            .port
            .list_runs(
                &call.ctx,
                RunQuery {
                    process,
                    state: state_filter,
                    mine,
                    cursor,
                    limit,
                },
            )
            .await
            .map_err(|e| port_failure(OP, e, request_id))?;
        let body = cursor_page(&page, limit, run_json).ok_or_else(|| violation(OP, request_id))?;
        Ok(ok_json(body))
    }
    .await;
    done(result)
}

// ------------------------------------------------------------------------------------------
// run_get

async fn run_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    const OP: &str = "run_get";
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(OP, &state, &site, &headers)?;
        parse_query(OP, raw_query.as_deref(), &[], request_id)?;
        if !is_run_id(&id) {
            return Err(invalid(
                OP,
                "A run id is not in the documented form.",
                request_id,
            ));
        }
        let run = call
            .port
            .get_run(&call.ctx, &id)
            .await
            .map_err(|e| port_failure(OP, e, request_id))?;
        let data = run_json(&run).ok_or_else(|| violation(OP, request_id))?;
        Ok(ok_json(json!({ "data": data })))
    }
    .await;
    done(result)
}

// ------------------------------------------------------------------------------------------
// run_events

/// `after`: digits only, at most eighteen of them (so it fits an `i64` seq).
fn after_of(raw: &str) -> Option<u64> {
    if raw.is_empty() || raw.len() > 18 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u64>().ok()
}

async fn run_events(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    const OP: &str = "run_events";
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(OP, &state, &site, &headers)?;
        let query = parse_query(OP, raw_query.as_deref(), &["after", "limit"], request_id)?;
        let limit = limit_of(OP, &query, DEFAULT_EVENTS_PAGE, MAX_EVENTS_PAGE, request_id)?;
        let after = match query.get("after") {
            None => 0,
            Some(raw) => after_of(raw).ok_or_else(|| {
                invalid(OP, "`after` must be a non-negative integer.", request_id)
            })?,
        };
        if !is_run_id(&id) {
            return Err(invalid(
                OP,
                "A run id is not in the documented form.",
                request_id,
            ));
        }
        let events = call
            .port
            .run_events(&call.ctx, &id, after, limit)
            .await
            .map_err(|e| port_failure(OP, e, request_id))?;
        // The contract: `seq` starts at 1, rises, and the page never passes the limit.
        let ordered = events.iter().all(|e| e.seq > after && e.seq >= 1)
            && events.windows(2).all(|pair| match pair {
                [first, second] => second.seq > first.seq,
                _ => true,
            });
        if events.len() > limit as usize || !ordered {
            return Err(violation(OP, request_id));
        }
        let mut body = Map::new();
        body.insert(
            "data".to_owned(),
            Json::Array(events.iter().map(event_json).collect()),
        );
        if let Some(last) = events.last() {
            body.insert("next_after".to_owned(), Json::from(last.seq));
        }
        Ok(ok_json(Json::Object(body)))
    }
    .await;
    done(result)
}

// ------------------------------------------------------------------------------------------
// run_cancel

async fn run_cancel(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    const OP: &str = "run_cancel";
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(OP, &state, &site, &headers)?;
        if !is_run_id(&id) {
            return Err(invalid(
                OP,
                "A run id is not in the documented form.",
                request_id,
            ));
        }
        let (_, parsed) = read_json(OP, request, RUN_CANCEL_BODY_CAP, request_id).await?;
        let mut map = closed_object(OP, parsed, &["reason"], request_id)?;
        let reason = match map.remove("reason") {
            Some(Json::String(reason))
                if (1..=MAX_REASON_CHARS).contains(&reason.chars().count()) =>
            {
                reason
            }
            _ => {
                return Err(invalid(
                    OP,
                    "`reason` is required, 1 to 500 characters.",
                    request_id,
                ));
            }
        };
        let run = call
            .port
            .cancel_run(&call.ctx, &id, &reason)
            .await
            .map_err(|e| port_failure(OP, e, request_id))?;
        let data = run_json(&run).ok_or_else(|| violation(OP, request_id))?;
        Ok(ok_json(json!({ "data": data })))
    }
    .await;
    done(result)
}
