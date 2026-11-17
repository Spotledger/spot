//! The connector operations of native API 0.25.0 (WP-INT-05.T4): `connector_list`,
//! `connector_get`, `connector_invoke` and `connector_runs`. The unauthenticated ingress,
//! `connector_inbound`, is in [`inbound`].
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request and refuse a malformed one before the port is asked:
//! the connector and operation names, the closed query and body objects, the `Idempotency-Key`
//! and every bound the contract names. They never decide who the caller is, which connectors the
//! caller may see or whether the caller may invoke an operation: the [`ConnectorsPort`] answers
//! for the caller its credential names on the site the request resolved to, and the routes pass
//! it no user, no role and no tenant. Input is validated against the operation's JSON Schema by
//! the port (`422 connector.input_invalid`, paths only, never values); the routes bound its size.
//!
//! * **Fail closed.** With no port every operation answers `501 service.not_configured` (never an
//!   empty `200`); a port failure that is not a declared refusal is `500 http.internal` with no
//!   detail; a port answer outside the contract's bounds is `500` too.
//! * **No existence oracle.** An unknown connector and one the caller may not see are the port's
//!   one answer, `404 connector.not_found`.
//! * **No secret leaves.** The port's types have no field for a credential, a secret reference
//!   or an endpoint secret, so none can be echoed; the run log has no input or output.
//! * **Logging.** A refused request is logged with the operation id and the problem code only; no
//!   body, input, output or token is logged.
//!
//! Every response is `private, no-store`.
//!
//! # The ingress group
//!
//! `connector_inbound` has `security: []`, so the registry would have to mount it as a public
//! group in a file of its own (a group is wholly public or wholly authenticated). This task owns
//! no such file, so [`inbound`] exposes [`inbound::registration`] and the routes; the registry
//! file `routes/connector_inbound.rs` (a few lines that re-export them under the names `SERVES`,
//! `PUBLIC`, `required_ports` and `routes`) is listed in the task report.
#![allow(clippy::result_large_err)] // a refusal is a whole response, built once and returned

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, HttpBody as _};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value as Json, json};

use super::RouteState;
use crate::api::ports::PortKey;
use crate::api::ports::connectors::{ConnectorPortError, ConnectorsPort, PageIn};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{REPLAYED_HEADER, build_ctx, render_error, take_body};

#[path = "connectors/inbound.rs"]
pub mod inbound;
#[path = "connectors/shapes.rs"]
mod shapes;

/// The operation ids this group serves. `connector_inbound` is a public group of its own
/// ([`inbound`]).
pub const SERVES: &[&str] = &[
    "connector_list",
    "connector_get",
    "connector_invoke",
    "connector_runs",
];

/// The ports this group needs: the connector port, whose adapter decides identity, visibility,
/// the right to invoke or to read a run log, the replay of a repeated key and the audit. A group
/// without it is not mounted.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn ConnectorsPort>()];
    &KEYS
}

/// The connector routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/connectors", get(connector_list))
        .route("/api/v1/connectors/{name}", get(connector_get))
        .route(
            "/api/v1/connectors/{name}/operations/{op}",
            post(connector_invoke),
        )
        .route("/api/v1/connectors/{name}/runs", get(connector_runs))
}

/// The contract's `limit` default.
const DEFAULT_LIMIT: usize = 50;
/// The contract's `limit` maximum.
const MAX_LIMIT: usize = 100;
/// The contract's `cursor` maximum length.
const MAX_CURSOR: usize = 2048;
/// The contract's bound on a name in a path.
const MAX_NAME: usize = 200;
/// The contract's bound on the members of `input`.
const MAX_INPUT_PROPERTIES: usize = 200;
/// The largest body `connector_invoke` reads.
const MAX_BODY_BYTES: usize = 256 * 1024;

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

/// A rendered refusal, logged by operation id and problem code only.
fn refuse(operation: &'static str, error: ApiError, request_id: &RequestId) -> Response {
    tracing::info!(
        operation,
        code = error.code.as_str(),
        "connector request refused"
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

/// `501 service.not_configured`: the site has no connector service.
pub(crate) fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no connector service configured.",
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
fn port_error(error: ConnectorPortError) -> ApiError {
    match error {
        ConnectorPortError::NotFound => ApiError::from_code(Code::ConnectorNotFound),
        ConnectorPortError::Disabled => ApiError::from_code(Code::ConnectorDisabled),
        ConnectorPortError::OperationUnknown => {
            ApiError::from_code(Code::ConnectorOperationUnknown)
        }
        ConnectorPortError::InputInvalid { fields } => {
            // Paths only: anything that is not a short pointer is dropped, so a port that put a
            // value in a path cannot make the route echo it.
            let paths: Vec<Json> = fields
                .into_iter()
                .filter(|f| f.starts_with('/') && f.chars().count() <= 200)
                .filter(|f| !f.chars().any(char::is_control))
                .take(50)
                .map(Json::from)
                .collect();
            ApiError::from_code(Code::ConnectorInputInvalid).detail("fields", Json::Array(paths))
        }
        ConnectorPortError::UpstreamFailed => ApiError::from_code(Code::ConnectorUpstreamFailed),
        ConnectorPortError::DeadlineExceeded => {
            ApiError::from_code(Code::ConnectorDeadlineExceeded)
        }
        ConnectorPortError::PermissionDenied => ApiError::from_code(Code::PermissionDenied),
        ConnectorPortError::IdempotencyConflict => {
            ApiError::from_code(Code::ApiIdempotencyConflict)
        }
        ConnectorPortError::CursorInvalid => invalid("`cursor` is not one this service issued."),
        ConnectorPortError::Unauthenticated => ApiError::from_code(Code::AuthSessionExpired),
        ConnectorPortError::Unavailable => ApiError::from_code(Code::ServerUnavailable),
        ConnectorPortError::Internal => ApiError::from_code(Code::HttpInternal),
    }
}

/// A name in a path: 1 to 200 characters, no control character.
fn is_name(raw: &str) -> bool {
    (1..=MAX_NAME).contains(&raw.chars().count()) && !raw.chars().any(char::is_control)
}

/// The contract's `Idempotency-Key`: 1 to 128 visible ASCII characters.
fn is_idempotency_key(raw: &str) -> bool {
    (1..=128).contains(&raw.len()) && raw.bytes().all(|b| (0x21..=0x7e).contains(&b))
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

/// The `cursor` and `limit` of a list, each at most once, nothing else.
fn parse_page(query: &str) -> Result<PageIn, ApiError> {
    let mut cursor: Option<String> = None;
    let mut limit: Option<usize> = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "cursor" => {
                if cursor.is_some() {
                    return Err(invalid("`cursor` can be given only once."));
                }
                let decoded = percent_decode(value)
                    .filter(|v| (1..=MAX_CURSOR).contains(&v.len()))
                    .filter(|v| v.bytes().all(|b| (0x21..=0x7e).contains(&b)))
                    .ok_or_else(|| invalid("`cursor` must be a cursor this service issued."))?;
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
                    "`cursor` and `limit` are the only query parameters.",
                ));
            }
        }
    }
    Ok(PageIn {
        cursor,
        limit: limit.unwrap_or(DEFAULT_LIMIT),
    })
}

/// The port and the request context of an authenticated call, or the answer to give without.
struct Call {
    port: Arc<dyn ConnectorsPort>,
    ctx: RequestCtx,
}

fn call(
    state: &RouteState,
    site: &SiteContext,
    headers: &HeaderMap,
    operation: &'static str,
) -> Result<Call, Response> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn ConnectorsPort>() else {
        return Err(not_configured(request_id));
    };
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(refuse(
            operation,
            ApiError::from_code(Code::AuthRequired),
            request_id,
        ));
    }
    Ok(Call { port, ctx })
}

fn names_ok(
    names: &[&str],
    operation: &'static str,
    request_id: &RequestId,
) -> Result<(), Response> {
    if names.iter().all(|n| is_name(n)) {
        Ok(())
    } else {
        Err(refuse(
            operation,
            invalid("A connector and an operation name are 1 to 200 characters."),
            request_id,
        ))
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

/// The `{ "input": {..} }` body: closed, the input an object of at most 200 members. The size
/// is checked before anything is buffered.
async fn input_body(
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
    let Json::Object(mut map) = body else {
        return Err(refuse(
            operation,
            invalid("The request body must be a JSON object."),
            request_id,
        ));
    };
    if map.keys().any(|k| k != "input") {
        return Err(refuse(
            operation,
            invalid("The body has a member this operation does not take."),
            request_id,
        ));
    }
    match map.remove("input") {
        Some(Json::Object(input)) if input.len() <= MAX_INPUT_PROPERTIES => Ok(input),
        _ => Err(refuse(
            operation,
            invalid("`input` is required: an object of at most 200 members."),
            request_id,
        )),
    }
}

fn internal(operation: &'static str, request_id: &RequestId) -> Response {
    tracing::error!(
        operation,
        "the connector port returned an answer outside the contract"
    );
    refuse(
        operation,
        ApiError::from_code(Code::HttpInternal),
        request_id,
    )
}

async fn connector_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    const OP: &str = "connector_list";
    let request_id = &site.request_id;
    // Read before the block so the future holds no reference to the (non-`Sync`) request.
    let query_text = request.uri().query().unwrap_or("").to_owned();
    drop(request);
    let response = async {
        let call = call(&state, &site, &headers, OP)?;
        let page = parse_page(&query_text).map_err(|e| refuse(OP, e, request_id))?;
        let out = call
            .port
            .list(&call.ctx, &page)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        if out.connectors.len() > page.limit
            || out
                .next_cursor
                .as_deref()
                .is_some_and(|c| !(1..=MAX_CURSOR).contains(&c.len()))
        {
            return Err(internal(OP, request_id));
        }
        let mut data = Vec::with_capacity(out.connectors.len());
        for view in &out.connectors {
            match shapes::connector_json(view) {
                Some(item) => data.push(item),
                None => return Err(internal(OP, request_id)),
            }
        }
        Ok::<Response, Response>(
            (
                StatusCode::OK,
                axum::Json(json!({ "data": data, "next_cursor": out.next_cursor })),
            )
                .into_response(),
        )
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn connector_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    const OP: &str = "connector_get";
    let request_id = &site.request_id;
    let response = async {
        let call = call(&state, &site, &headers, OP)?;
        names_ok(&[name.as_str()], OP, request_id)?;
        let view = call
            .port
            .get(&call.ctx, &name)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        let Some(data) = shapes::connector_json(&view) else {
            return Err(internal(OP, request_id));
        };
        Ok::<Response, Response>(
            (StatusCode::OK, axum::Json(json!({ "data": data }))).into_response(),
        )
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn connector_invoke(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((name, op)): Path<(String, String)>,
    request: Request,
) -> Response {
    const OP: &str = "connector_invoke";
    let request_id = &site.request_id;
    let response = async {
        let authed = call(&state, &site, &headers, OP)?;
        names_ok(&[name.as_str(), op.as_str()], OP, request_id)?;
        let key = idempotency_key(&headers, OP, request_id)?;
        let input = input_body(request, OP, request_id).await?;
        let mut ctx = authed.ctx;
        ctx.idempotency_key = Some(key);
        let out = authed
            .port
            .invoke(&ctx, &name, &op, &input)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        let Some(data) = shapes::invoke_json(&out) else {
            return Err(internal(OP, request_id));
        };
        let status = if out.job_id.is_some() {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        };
        let mut response = (status, axum::Json(json!({ "data": data }))).into_response();
        if out.replayed {
            response
                .headers_mut()
                .insert(REPLAYED_HEADER, HeaderValue::from_static("true"));
        }
        Ok::<Response, Response>(response)
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}

async fn connector_runs(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    const OP: &str = "connector_runs";
    let request_id = &site.request_id;
    let query_text = request.uri().query().unwrap_or("").to_owned();
    drop(request);
    let response = async {
        let call = call(&state, &site, &headers, OP)?;
        names_ok(&[name.as_str()], OP, request_id)?;
        let page = parse_page(&query_text).map_err(|e| refuse(OP, e, request_id))?;
        let out = call
            .port
            .runs(&call.ctx, &name, &page)
            .await
            .map_err(|e| refuse(OP, port_error(e), request_id))?;
        if out.runs.len() > page.limit
            || out
                .next_cursor
                .as_deref()
                .is_some_and(|c| !(1..=MAX_CURSOR).contains(&c.len()))
        {
            return Err(internal(OP, request_id));
        }
        let mut data = Vec::with_capacity(out.runs.len());
        for run in &out.runs {
            match shapes::run_json(run) {
                Some(item) => data.push(item),
                None => return Err(internal(OP, request_id)),
            }
        }
        Ok::<Response, Response>(
            (
                StatusCode::OK,
                axum::Json(json!({ "data": data, "next_cursor": out.next_cursor })),
            )
                .into_response(),
        )
    }
    .await;
    private_no_store(response.unwrap_or_else(|r| r))
}
