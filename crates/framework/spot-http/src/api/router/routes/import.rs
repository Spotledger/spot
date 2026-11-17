//! `/api/v1/import*` (contract 0.21.0, WP-CON-03.T50): the data import operations
//! (`import_limits`, `import_create`, `import_get`, `import_errors`, `import_run`,
//! `import_cancel`) over an [`ImportPort`].
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request (the body's keys and types, the bounds the contract
//! states, the characters of an id, a cursor or an `Idempotency-Key`) and refuse a malformed one
//! before the port is asked. They never decide who the caller is, whether the caller holds the
//! import or submit right, whether a file is the caller's and clean, or whether a job is the
//! caller's: the port answers for the caller its credential names, and the routes pass it no
//! user and no tenant. A job that is not the caller's is the port's `404 document.not_found`,
//! the same answer as a missing one.
//!
//! The routes never echo a request body, a file id or a cell in an error. They do not use the
//! doctype or field names for anything but passing them on. An error the port raises that is not
//! one of the contract's codes for the operation is `500 http.internal` with no detail.
//!
//! # Idempotency
//!
//! A registered group sits outside the router's `Idempotency-Key` middleware. `import_create`
//! and `import_run` honour the key (the contract's optional parameter): the route validates its
//! shape (1 to 128 visible ASCII characters, else `400 api.invalid_request`) and passes it in
//! `ctx.idempotency_key`; replaying the first answer is the port's job (see the port's docs).
//! `import_cancel` takes none.
//!
//! Every response is `private, no-store`: a job and its errors belong to the caller.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value as Json, json};
use spot_storage_contract::Name;

use super::{Registration, RouteState};
use crate::api::codec::parse_record_id;
use crate::api::ports::PortKey;
use crate::api::ports::import::{
    DEFAULT_ERRORS_PAGE, ImportCreateIn, ImportErrorsPageOut, ImportJobOut, ImportLimitsOut,
    ImportMappingIn, ImportModeIn, ImportPort, MAX_CELL_TEXT, MAX_CHILD_TABLES, MAX_ERRORS_PAGE,
    MAX_MAPPING_COLUMNS,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, ErrorCode, RequestCtx};
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail, take_body};

/// The operation ids this group serves.
pub const SERVES: &[&str] = &[
    "import_limits",
    "import_create",
    "import_get",
    "import_errors",
    "import_run",
    "import_cancel",
];

/// The ports this group needs: the import port, whose adapter decides identity, the import and
/// submit rights, file ownership, the field guard on cells, limits and audit. A group without it
/// is not mounted at all.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn ImportPort>()];
    &KEYS
}

/// The import routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/import/limits", get(import_limits))
        .route("/api/v1/import", post(import_create))
        .route("/api/v1/import/{id}", get(import_get))
        .route("/api/v1/import/{id}/errors", get(import_errors))
        .route("/api/v1/import/{id}/run", post(import_run))
        .route("/api/v1/import/{id}/cancel", post(import_cancel))
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "import",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

/// The contract's bounds.
const MAX_NAME: usize = 200;
const MAX_ID: usize = 200;
const MAX_CURSOR: usize = 2048;
const MAX_EXPLANATION: usize = 500;
const DIGEST_LEN: usize = 64;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn identifier(text: &str) -> bool {
    Name::new(text).is_ok()
}

/// A text of 1 to `max` characters with no control character.
fn plain_text(text: &str, max: usize) -> bool {
    !text.is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control)
}

fn object_of<'a>(
    body: &'a Json,
    allowed: &[&str],
    required: &[&str],
) -> Result<&'a Map<String, Json>, ApiError> {
    let map = body
        .as_object()
        .ok_or_else(|| invalid("The request body must be a JSON object."))?;
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(
            "The request body has a field this operation does not accept.",
        ));
    }
    if let Some(missing) = required.iter().find(|k| !map.contains_key(**k)) {
        return Err(invalid(format!("The request body needs `{missing}`.")));
    }
    Ok(map)
}

fn bool_of(map: &Map<String, Json>, key: &str, default: bool) -> Result<bool, ApiError> {
    match map.get(key) {
        None => Ok(default),
        Some(Json::Bool(b)) => Ok(*b),
        Some(_) => Err(invalid(format!("`{key}` must be true or false."))),
    }
}

fn mapping_of(value: &Json) -> Result<ImportMappingIn, ApiError> {
    let map = object_of(value, &["columns", "child_tables"], &["columns"])?;
    let items = map
        .get("columns")
        .and_then(Json::as_array)
        .ok_or_else(|| invalid("`mapping.columns` must be an array."))?;
    if items.len() > MAX_MAPPING_COLUMNS {
        return Err(invalid(format!(
            "`mapping.columns` has {} entries; the most is {MAX_MAPPING_COLUMNS}.",
            items.len()
        )));
    }
    let mut columns = Vec::with_capacity(items.len());
    for item in items {
        let pair = object_of(item, &["column", "field"], &["column", "field"])?;
        let column = pair
            .get("column")
            .and_then(Json::as_str)
            .filter(|c| plain_text(c, MAX_NAME))
            .ok_or_else(|| invalid("A mapping `column` must be 1 to 200 characters of text."))?;
        let field = pair
            .get("field")
            .and_then(Json::as_str)
            .filter(|f| f.len() <= MAX_NAME && identifier(f))
            .ok_or_else(|| invalid("A mapping `field` must be a field name."))?;
        columns.push((column.to_owned(), field.to_owned()));
    }
    let mut child_tables = std::collections::BTreeMap::new();
    if let Some(value) = map.get("child_tables") {
        let tables = value
            .as_object()
            .ok_or_else(|| invalid("`mapping.child_tables` must be an object."))?;
        if tables.len() > MAX_CHILD_TABLES {
            return Err(invalid(format!(
                "`mapping.child_tables` has {} entries; the most is {MAX_CHILD_TABLES}.",
                tables.len()
            )));
        }
        for (table, prefix) in tables {
            let prefix = prefix
                .as_str()
                .filter(|p| p.chars().count() <= MAX_NAME && !p.chars().any(char::is_control))
                .ok_or_else(|| {
                    invalid("A child table prefix must be text of at most 200 characters.")
                })?;
            if !identifier(table) {
                return Err(invalid("A child table must be a field name."));
            }
            child_tables.insert(table.clone(), prefix.to_owned());
        }
    }
    Ok(ImportMappingIn {
        columns,
        child_tables,
    })
}

fn create_of(body: &Json) -> Result<ImportCreateIn, ApiError> {
    let map = object_of(
        body,
        &["file_id", "doctype", "mode", "mapping", "submit", "dry_run"],
        &["file_id", "doctype", "mode"],
    )?;
    let file_id = map
        .get("file_id")
        .and_then(Json::as_str)
        .filter(|id| parse_record_id(id).is_some())
        .ok_or_else(|| invalid("`file_id` must be the id of an uploaded file."))?;
    let doctype = map
        .get("doctype")
        .and_then(Json::as_str)
        .filter(|d| identifier(d))
        .ok_or_else(|| invalid("`doctype` must be a DocType storage name."))?;
    let mode = map
        .get("mode")
        .and_then(Json::as_str)
        .and_then(ImportModeIn::parse)
        .ok_or_else(|| invalid("`mode` must be `insert`, `update` or `upsert`."))?;
    let mapping = match map.get("mapping") {
        None => None,
        Some(value) => Some(mapping_of(value)?),
    };
    Ok(ImportCreateIn {
        file_id: file_id.to_owned(),
        doctype: doctype.to_owned(),
        mode,
        mapping,
        submit: bool_of(map, "submit", false)?,
        dry_run: bool_of(map, "dry_run", true)?,
    })
}

fn digest_of(body: &Json) -> Result<String, ApiError> {
    let map = object_of(body, &["digest"], &["digest"])?;
    map.get("digest")
        .and_then(Json::as_str)
        .filter(|d| d.len() == DIGEST_LEN && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned)
        .ok_or_else(|| invalid("`digest` must be the 64 hexadecimal characters of a dry run."))
}

/// A job id in the path: a bounded token that cannot be a path, a header or a query.
fn job_id(raw: &str) -> Result<(), ApiError> {
    if !raw.is_empty()
        && raw.len() <= MAX_ID
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(invalid("The job id is not valid."))
    }
}

/// The `Idempotency-Key` of the request, when sent: 1 to 128 visible ASCII characters.
fn key_of(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(raw) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let bad = || invalid("The Idempotency-Key header must be 1 to 128 visible ASCII characters.");
    let key = raw.to_str().map_err(|_| bad())?;
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(bad());
    }
    Ok(Some(key.to_owned()))
}

/// The codes a port may answer these operations with; any other error is `http.internal`.
fn contract_code(code: Code) -> bool {
    matches!(
        code,
        Code::ApiInvalidRequest
            | Code::ApiIdempotencyConflict
            | Code::ApiRateLimited
            | Code::AuthRequired
            | Code::PermissionDenied
            | Code::DocumentNotFound
            | Code::ImportFileUnreadable
            | Code::ImportMappingInvalid
            | Code::ImportLimitExceeded
            | Code::ImportNotReady
            | Code::ImportDigestChanged
            | Code::ImportCancelled
            | Code::ImportRightMissing
            | Code::ServerUnavailable
            | Code::HttpInternal
    )
}

/// The port's error as the response gives it: a contract code stays, anything else is the
/// internal error with no detail (never the port's text).
fn mapped(error: ApiError) -> ApiError {
    let keep = matches!(&error.code, ErrorCode::Registry(code) if contract_code(*code));
    if keep {
        error
    } else {
        ApiError::from_code(Code::HttpInternal)
    }
}

struct Call {
    port: std::sync::Arc<dyn ImportPort>,
    ctx: RequestCtx,
}

fn call(
    state: &RouteState,
    site: &SiteContext,
    headers: &HeaderMap,
    keyed: bool,
) -> Result<Call, Failure> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn ImportPort>() else {
        return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
    };
    let key = if keyed {
        key_of(headers).map_err(|e| fail(e, request_id))?
    } else {
        None
    };
    let ctx = build_ctx(site, headers, key);
    if ctx.credential.is_none() {
        return Err(fail(ApiError::from_code(Code::AuthRequired), request_id));
    }
    Ok(Call { port, ctx })
}

type Handled = Result<Response, Failure>;

fn done(result: Handled) -> Response {
    let mut response = match result {
        Ok(response) => response,
        Err(failure) => failure.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

/// Cuts `text` to at most `max` bytes at a character boundary.
fn cut_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

fn cut_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn mapping_json(mapping: &ImportMappingIn) -> Json {
    let mut map = Map::new();
    map.insert(
        "columns".to_owned(),
        Json::Array(
            mapping
                .columns
                .iter()
                .map(|(column, field)| json!({"column": column, "field": field}))
                .collect(),
        ),
    );
    if !mapping.child_tables.is_empty() {
        map.insert("child_tables".to_owned(), json!(mapping.child_tables));
    }
    Json::Object(map)
}

fn job_json(job: &ImportJobOut) -> Json {
    let mut map = Map::new();
    map.insert("id".to_owned(), json!(job.id));
    map.insert("doctype".to_owned(), json!(job.doctype));
    map.insert("mode".to_owned(), json!(job.mode.as_str()));
    map.insert("state".to_owned(), json!(job.state.as_str()));
    if let Some(mapping) = &job.mapping {
        map.insert("mapping".to_owned(), mapping_json(mapping));
    }
    if let Some(submit) = job.submit {
        map.insert("submit".to_owned(), Json::Bool(submit));
    }
    map.insert(
        "counts".to_owned(),
        json!({
            "rows": job.counts.rows,
            "ok": job.counts.ok,
            "errors": job.counts.errors,
            "skipped": job.counts.skipped,
        }),
    );
    map.insert("progress".to_owned(), json!(job.progress.min(1000)));
    map.insert(
        "digest".to_owned(),
        job.digest.as_ref().map_or(Json::Null, |d| json!(d)),
    );
    map.insert("created_by".to_owned(), json!(job.created_by));
    map.insert("created_at".to_owned(), json!(job.created_at));
    Json::Object(map)
}

fn errors_json(page: &ImportErrorsPageOut) -> Json {
    let data: Vec<Json> = page
        .rows
        .iter()
        .map(|row| {
            let mut map = Map::new();
            map.insert("row".to_owned(), json!(row.row.max(1)));
            if let Some(column) = &row.column {
                map.insert("column".to_owned(), json!(cut_chars(column, MAX_NAME)));
            }
            // A cell is cut to 256 bytes here, whatever the port sent; the port leaves it out
            // when the caller could not write the field.
            if let Some(cell) = &row.cell_text {
                map.insert(
                    "cell_text".to_owned(),
                    json!(cut_bytes(cell, MAX_CELL_TEXT)),
                );
            }
            map.insert("code".to_owned(), json!(cut_chars(&row.code, 100)));
            map.insert(
                "explanation".to_owned(),
                json!(cut_chars(&row.explanation, MAX_EXPLANATION)),
            );
            Json::Object(map)
        })
        .collect();
    json!({"data": data, "next_cursor": page.next_cursor})
}

fn limits_json(limits: &ImportLimitsOut) -> Json {
    json!({"data": {
        "max_rows": limits.max_rows,
        "max_bytes": limits.max_bytes,
        "max_columns": limits.max_columns,
        "modes": limits.modes.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
    }})
}

fn accepted(job: &ImportJobOut) -> Response {
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({"data": job_json(job)})),
    )
        .into_response()
}

fn ok_job(job: &ImportJobOut) -> Response {
    (StatusCode::OK, axum::Json(json!({"data": job_json(job)}))).into_response()
}

async fn import_limits(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, false)?;
        let limits = call
            .port
            .limits(&call.ctx)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok((StatusCode::OK, axum::Json(limits_json(&limits))).into_response())
    }
    .await;
    done(result)
}

async fn import_create(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, true)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let create = create_of(&body).map_err(|e| fail(e, request_id))?;
        let job = call
            .port
            .create(&call.ctx, create)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok(accepted(&job))
    }
    .await;
    done(result)
}

async fn import_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, false)?;
        job_id(&id).map_err(|e| fail(e, request_id))?;
        let job = call
            .port
            .get(&call.ctx, &id)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok(ok_job(&job))
    }
    .await;
    done(result)
}

async fn import_errors(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, false)?;
        job_id(&id).map_err(|e| fail(e, request_id))?;
        if query.keys().any(|k| k != "cursor" && k != "limit") {
            return Err(fail(
                invalid("This operation does not accept that query parameter."),
                request_id,
            ));
        }
        let limit = match query.get("limit") {
            None => DEFAULT_ERRORS_PAGE,
            Some(text) => text
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=MAX_ERRORS_PAGE).contains(n))
                .ok_or_else(|| {
                    fail(
                        invalid(format!(
                            "`limit` must be a whole number from 1 to {MAX_ERRORS_PAGE}."
                        )),
                        request_id,
                    )
                })?,
        };
        let cursor = match query.get("cursor") {
            None => None,
            Some(text)
                if !text.is_empty()
                    && text.len() <= MAX_CURSOR
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) =>
            {
                Some(text.as_str())
            }
            Some(_) => {
                return Err(fail(
                    invalid("The cursor is not one this server issued."),
                    request_id,
                ));
            }
        };
        let page = call
            .port
            .errors(&call.ctx, &id, cursor, limit)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok((StatusCode::OK, axum::Json(errors_json(&page))).into_response())
    }
    .await;
    done(result)
}

async fn import_run(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, true)?;
        job_id(&id).map_err(|e| fail(e, request_id))?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let digest = digest_of(&body).map_err(|e| fail(e, request_id))?;
        let job = call
            .port
            .run(&call.ctx, &id, &digest)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok(accepted(&job))
    }
    .await;
    done(result)
}

async fn import_cancel(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, false)?;
        job_id(&id).map_err(|e| fail(e, request_id))?;
        let job = call
            .port
            .cancel(&call.ctx, &id)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok(ok_job(&job))
    }
    .await;
    done(result)
}
