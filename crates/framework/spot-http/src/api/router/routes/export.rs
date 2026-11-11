//! `/api/v1/export*` (contract 0.4.0, WP-INT-03.T2): bulk export of one DocType in bounded
//! chunks (`export_start`, `export_next`) and incremental extraction with tombstones
//! (`export_extract`).
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request (the body's keys and types, the bounds the contract
//! states, the characters of a cursor or watermark) and refuse a malformed one before the port
//! is asked. They never decide who the caller is, whether the caller holds the `export` right,
//! which rows or columns the caller may take, or whether a cursor belongs to the caller: the
//! port answers for the caller its credential names, and the routes pass it no user and no
//! tenant. A cursor and a watermark are opaque to the routes.
//!
//! # The response
//!
//! A chunk answers `200` with the export's content type (`text/csv; charset=utf-8` or
//! `application/x-ndjson`), or `multipart/mixed` when the export includes child tables (one
//! part per table in the chunk, `name` in each part's `Content-Disposition`, the boundary a
//! digest of the bytes so the same chunk is the same bytes). `X-Export-Rows` and
//! `X-Export-Watermark` are on every chunk; `X-Export-Cursor` is absent on the last one. Every
//! response is `private, no-store`: an export is the caller's data and a cursor is a bearer
//! of a position.
//!
//! **Not registered in the route registry yet.** This file is a helper module: it does not
//! declare the three registration items (`SERVES`, `routes`, `required_ports`), because
//! registering the group adds its operations to the served set and the conformance walk
//! (`tests/conformance.rs`, not owned by this task) then needs a case for each of them. The
//! host mounts the group itself (`services/export.rs`) through [`registration`]. To register
//! it, rename [`EXPORT_SERVES`], [`export_routes`] and [`export_required_ports`] to `SERVES`,
//! `routes` and `required_ports` and add the walk cases.

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Map, Value as Json, json};
use sha2::{Digest as _, Sha256};
use spot_storage_contract::Name;

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::export::{
    BulkPort, ExportChunkOut, ExportFormatIn, ExportStartIn, ExtractIn, ExtractPageOut, MAX_TOKEN,
    OrderKeyIn,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail, take_body};

/// The operation ids this group serves once registered.
pub const EXPORT_SERVES: &[&str] = &["export_start", "export_next", "export_extract"];

/// The ports this group needs: the bulk port, whose adapter decides identity, the export right,
/// the field guard, cursors, limits and audit. A group without it is not mounted at all.
pub fn export_required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn BulkPort>()];
    &KEYS
}

/// The export routes.
pub fn export_routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/export", post(export_start))
        .route("/api/v1/export/next", post(export_next))
        .route("/api/v1/export/extract", post(export_extract))
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "export",
        serves: EXPORT_SERVES,
        required_ports: export_required_ports,
        routes: export_routes,
        public: None,
    }
}

/// The response headers of the contract.
const CURSOR_HEADER: &str = "x-export-cursor";
const ROWS_HEADER: &str = "x-export-rows";
const WATERMARK_HEADER: &str = "x-export-watermark";

/// The contract's bounds.
const MAX_COLUMNS: usize = 256;
const MAX_ORDER_KEYS: usize = 8;
const MAX_EXTRACT_LIMIT: u64 = 5_000;
const DEFAULT_EXTRACT_LIMIT: u32 = 1_000;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

/// Whether `text` is a valid storage name (the contract's `Identifier`).
fn identifier(text: &str) -> bool {
    Name::new(text).is_ok()
}

/// Whether `text` can be a cursor or a watermark: 1 to 2,048 URL-safe base64 characters or the
/// dots and letters of a versioned prefix. A token that is not of this shape cannot be one this
/// server issued, so it never reaches the port.
fn token_shape(text: &str) -> bool {
    (1..=MAX_TOKEN).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Reads a JSON object that has exactly the keys in `allowed` (and at least `required`).
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

fn doctype_of(map: &Map<String, Json>) -> Result<String, ApiError> {
    let doctype = map
        .get("doctype")
        .and_then(Json::as_str)
        .filter(|d| identifier(d))
        .ok_or_else(|| invalid("`doctype` must be a DocType storage name."))?;
    Ok(doctype.to_owned())
}

fn columns_of(map: &Map<String, Json>) -> Result<Vec<String>, ApiError> {
    let Some(value) = map.get("columns") else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| invalid("`columns` must be an array of field names."))?;
    if items.len() > MAX_COLUMNS {
        return Err(invalid(format!(
            "`columns` has {} entries; the most is {MAX_COLUMNS}.",
            items.len()
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let name = item
            .as_str()
            .filter(|n| identifier(n))
            .ok_or_else(|| invalid("`columns` must be an array of field names."))?;
        if out.iter().any(|c| c == name) {
            return Err(invalid("`columns` names a field twice."));
        }
        out.push(name.to_owned());
    }
    Ok(out)
}

fn order_of(map: &Map<String, Json>) -> Result<Vec<OrderKeyIn>, ApiError> {
    let Some(value) = map.get("order") else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| invalid("`order` must be an array of order keys."))?;
    if items.len() > MAX_ORDER_KEYS {
        return Err(invalid(format!(
            "`order` has {} keys; the most is {MAX_ORDER_KEYS}.",
            items.len()
        )));
    }
    items
        .iter()
        .map(|item| {
            let key = object_of(item, &["field", "dir", "nulls"], &["field"])?;
            let field = key
                .get("field")
                .and_then(Json::as_str)
                .filter(|f| identifier(f))
                .ok_or_else(|| invalid("an order key needs a `field` that is a field name."))?;
            let descending = match key.get("dir").and_then(Json::as_str) {
                None | Some("asc") => false,
                Some("desc") => true,
                Some(_) => return Err(invalid("`dir` must be `asc` or `desc`.")),
            };
            let nulls_first = match key.get("nulls") {
                None => None,
                Some(Json::String(n)) if n == "first" => Some(true),
                Some(Json::String(n)) if n == "last" => Some(false),
                Some(_) => return Err(invalid("`nulls` must be `first` or `last`.")),
            };
            Ok(OrderKeyIn {
                field: field.to_owned(),
                descending,
                nulls_first,
            })
        })
        .collect()
}

fn start_of(body: &Json) -> Result<ExportStartIn, ApiError> {
    let map = object_of(
        body,
        &[
            "doctype",
            "columns",
            "filter",
            "order",
            "format",
            "include_children",
            "max_rows",
        ],
        &["doctype", "format"],
    )?;
    let format = match map.get("format").and_then(Json::as_str) {
        Some("csv") => ExportFormatIn::Csv,
        Some("ndjson") => ExportFormatIn::Ndjson,
        _ => return Err(invalid("`format` must be `csv` or `ndjson`.")),
    };
    let filter = match map.get("filter") {
        None => None,
        Some(value) if value.is_object() => Some(value.clone()),
        Some(_) => return Err(invalid("`filter` must be a filter object.")),
    };
    let include_children = match map.get("include_children") {
        None => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => return Err(invalid("`include_children` must be true or false.")),
    };
    let max_rows = match map.get("max_rows") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|n| *n >= 1)
                .ok_or_else(|| invalid("`max_rows` must be a whole number of at least 1."))?,
        ),
    };
    Ok(ExportStartIn {
        doctype: doctype_of(map)?,
        columns: columns_of(map)?,
        filter,
        order: order_of(map)?,
        format,
        include_children,
        max_rows,
    })
}

fn extract_of(body: &Json) -> Result<ExtractIn, ApiError> {
    let map = object_of(
        body,
        &["doctype", "columns", "watermark", "limit"],
        &["doctype", "watermark"],
    )?;
    let watermark = map
        .get("watermark")
        .and_then(Json::as_str)
        .filter(|w| token_shape(w))
        .ok_or_else(|| {
            ApiError::new(
                Code::BulkCursorInvalid,
                "The watermark is not one this server issued.",
            )
        })?;
    let limit = match map.get("limit") {
        None => DEFAULT_EXTRACT_LIMIT,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=MAX_EXTRACT_LIMIT).contains(n))
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                invalid(format!(
                    "`limit` must be a whole number from 1 to {MAX_EXTRACT_LIMIT}."
                ))
            })?,
    };
    Ok(ExtractIn {
        doctype: doctype_of(map)?,
        columns: columns_of(map)?,
        watermark: watermark.to_owned(),
        limit,
    })
}

/// The port and the request context, or the answer to give without them.
struct Call {
    port: std::sync::Arc<dyn BulkPort>,
    ctx: RequestCtx,
}

fn call(state: &RouteState, site: &SiteContext, headers: &HeaderMap) -> Result<Call, Failure> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn BulkPort>() else {
        return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
    };
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(fail(ApiError::from_code(Code::AuthRequired), request_id));
    }
    Ok(Call { port, ctx })
}

type Handled = Result<Response, Failure>;

fn private_no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn done(result: Handled) -> Response {
    match result {
        Ok(response) => private_no_store(response),
        Err(failure) => private_no_store(failure.into_response()),
    }
}

/// The multipart boundary of `parts`: a digest of their bytes, extended until no part contains
/// it, so no cell can end a part early and the same chunk is the same bytes.
fn boundary(chunk: &ExportChunkOut) -> String {
    let mut hasher = Sha256::new();
    for part in &chunk.parts {
        hasher.update(part.name.as_bytes());
        hasher.update(&part.bytes);
    }
    let digest = hasher.finalize();
    let mut text = String::from("spot-export-");
    for byte in digest.iter().take(12) {
        text.push_str(&format!("{byte:02x}"));
    }
    let mut round = 0u32;
    while chunk
        .parts
        .iter()
        .any(|p| contains(&p.bytes, text.as_bytes()))
    {
        round += 1;
        text.push_str(&format!("-{round}"));
    }
    text
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// A part name is a table's storage name; anything else is refused rather than put in a header.
fn part_name(name: &str) -> &str {
    if identifier(name) { name } else { "part" }
}

fn chunk_response(chunk: ExportChunkOut) -> Response {
    let (content_type, body) = if chunk.multipart {
        let boundary = boundary(&chunk);
        let mut body = Vec::new();
        for part in &chunk.parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Type: {}\r\n", chunk.format.content_type()).as_bytes(),
            );
            body.extend_from_slice(
                format!(
                    "Content-Disposition: attachment; name=\"{}\"\r\n\r\n",
                    part_name(&part.name)
                )
                .as_bytes(),
            );
            body.extend_from_slice(&part.bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/mixed; boundary={boundary}"), body)
    } else {
        let bytes = chunk
            .parts
            .iter()
            .flat_map(|p| p.bytes.iter().copied())
            .collect::<Vec<u8>>();
        (chunk.format.content_type().to_owned(), bytes)
    };
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(
        HeaderName::from_static(ROWS_HEADER),
        HeaderValue::from(chunk.rows),
    );
    // A token the port made that cannot be a header value is dropped, never repaired: a
    // consumer then sees no cursor and no watermark, which it must treat as a failed export.
    if let Ok(value) = HeaderValue::from_str(&chunk.watermark) {
        headers.insert(HeaderName::from_static(WATERMARK_HEADER), value);
    }
    if let Some(cursor) = &chunk.cursor
        && let Ok(value) = HeaderValue::from_str(cursor)
    {
        headers.insert(HeaderName::from_static(CURSOR_HEADER), value);
    }
    response
}

fn page_json(page: &ExtractPageOut) -> Json {
    json!({
        "data": {
            "upserts": page.upserts,
            "deletes": page.deletes,
            "new_watermark": page.new_watermark,
            "has_more": page.has_more,
        }
    })
}

async fn export_start(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let spec = start_of(&body).map_err(|e| fail(e, request_id))?;
        let chunk = call
            .port
            .start(&call.ctx, spec)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok(chunk_response(chunk))
    }
    .await;
    done(result)
}

async fn export_next(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let map = object_of(&body, &["cursor"], &["cursor"]).map_err(|e| fail(e, request_id))?;
        // A cursor of the wrong shape is the same answer as a forged one: it does not say which.
        let cursor = map
            .get("cursor")
            .and_then(Json::as_str)
            .filter(|c| token_shape(c))
            .ok_or_else(|| {
                fail(
                    ApiError::new(
                        Code::BulkCursorInvalid,
                        "The cursor is not one this server issued.",
                    ),
                    request_id,
                )
            })?;
        let chunk = call
            .port
            .next(&call.ctx, cursor)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok(chunk_response(chunk))
    }
    .await;
    done(result)
}

async fn export_extract(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let spec = extract_of(&body).map_err(|e| fail(e, request_id))?;
        let page = call
            .port
            .extract(&call.ctx, spec)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok((StatusCode::OK, axum::Json(page_json(&page))).into_response())
    }
    .await;
    done(result)
}
