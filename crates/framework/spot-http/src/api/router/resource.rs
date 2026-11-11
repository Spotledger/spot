//! Meta and document endpoints: `GET /api/v1/meta/{doctype}`, and
//! `POST/GET /api/v1/resource/{doctype}`, `/query`, and `/{id}` update/delete.

use std::collections::BTreeMap;

use crate::api::codec::{MetaLookup, WriteMode, decode_body};
use crate::api::filter;
use crate::api::meta::DocTypeMeta;
use crate::api::registry::Code;
use crate::api::service::{ApiError, QuerySpec, is_lookup_capable_field};
use crate::request::RequestId;
use crate::site::SiteContext;
use axum::extract::{Path, Query, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value as Json;

use super::{
    Failure, HandlerResult, SharedState, build_ctx, checked_name, document_response, envelope,
    fail, json_response, lookup_over, matches_etag, page_response, parse_if_match, parse_limit,
    path_identifier, path_record_id, prepare_meta, take_body, unknown_field_error, validated_key,
};

pub(super) async fn meta_get(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(doctype): Path<String>,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    let meta = state
        .meta
        .doctype_meta(&ctx, &doctype)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    for field in &meta.fields {
        if field.has_lookup_override() {
            is_lookup_capable_field(field).map_err(|e| fail(e, &site.request_id))?;
        }
    }
    let etag = format!("\"{}\"", meta.version);
    if matches_etag(&headers, &etag) {
        let response = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag.clone())
            .body(axum::body::Body::empty())
            .unwrap_or_else(|_| StatusCode::NOT_MODIFIED.into_response());
        return Ok(response);
    }
    let body = envelope(meta.to_json(), &[]);
    let mut response = json_response(StatusCode::OK, body);
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// Resource: create / list / query / get / patch / delete

pub(super) async fn resource_create(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(doctype): Path<String>,
    request: Request,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let key = validated_key(&request);
    let ctx = build_ctx(&site, &headers, key);
    let body = take_body(request, true, &site.request_id).await?;
    let (meta, metas) = prepare_meta(&state, &ctx, &doctype)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let json = body.unwrap_or_else(|| Json::Object(serde_json::Map::new()));
    let doc = decode_body(&json, &meta, WriteMode::Create, None, lookup_over(&metas))
        .map_err(|e| fail(e, &site.request_id))?;
    let result = state
        .resource
        .create(&ctx, &doctype, &doc)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    document_response(
        StatusCode::CREATED,
        &result,
        &meta,
        &metas,
        Some(&doctype),
        &site.request_id,
    )
}

pub(super) async fn resource_get(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id)): Path<(String, String)>,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let id = path_record_id(&id, &site.request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    let result = state
        .resource
        .get(&ctx, &doctype, id)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let (meta, metas) = prepare_meta(&state, &ctx, &doctype)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    document_response(
        StatusCode::OK,
        &result,
        &meta,
        &metas,
        None,
        &site.request_id,
    )
}

pub(super) async fn resource_update(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id)): Path<(String, String)>,
    request: Request,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let id = path_record_id(&id, &site.request_id)?;
    let expected = parse_if_match(&headers, true, &site.request_id)?.ok_or_else(|| {
        fail(
            ApiError::from_code(Code::ApiPreconditionRequired),
            &site.request_id,
        )
    })?;
    let ctx = build_ctx(&site, &headers, None);
    let body = take_body(request, true, &site.request_id).await?;
    let (meta, metas) = prepare_meta(&state, &ctx, &doctype)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let json = body.unwrap_or_else(|| Json::Object(serde_json::Map::new()));
    let doc = decode_body(
        &json,
        &meta,
        WriteMode::Patch,
        Some(id),
        lookup_over(&metas),
    )
    .map_err(|e| fail(e, &site.request_id))?;
    let patch = doc.into_patch();
    let result = state
        .resource
        .update(&ctx, &doctype, id, &patch, expected)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    document_response(
        StatusCode::OK,
        &result,
        &meta,
        &metas,
        None,
        &site.request_id,
    )
}

pub(super) async fn resource_delete(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id)): Path<(String, String)>,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let id = path_record_id(&id, &site.request_id)?;
    let expected = parse_if_match(&headers, true, &site.request_id)?.ok_or_else(|| {
        fail(
            ApiError::from_code(Code::ApiPreconditionRequired),
            &site.request_id,
        )
    })?;
    let ctx = build_ctx(&site, &headers, None);
    state
        .resource
        .delete(&ctx, &doctype, id, expected)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) async fn resource_list(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(doctype): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    let (meta, metas) = prepare_meta(&state, &ctx, &doctype)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let limit = match query.get("limit") {
        None => 20,
        Some(raw) => {
            let n: i64 = raw.parse().map_err(|_| {
                fail(
                    ApiError::new(Code::ApiInvalidRequest, "`limit` must be an integer."),
                    &site.request_id,
                )
            })?;
            parse_limit(Some(n), &site.request_id)?
        }
    };
    let mut fields = Vec::new();
    if let Some(raw) = query.get("fields") {
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if meta.field(part).is_none() && part != "id" && part != "version" {
                return Err(fail(unknown_field_error(&meta, part), &site.request_id));
            }
            fields.push(checked_name(part, &site.request_id)?);
        }
    }
    let spec = QuerySpec {
        fields,
        filter: None,
        order: Vec::new(),
        limit,
        cursor: query.get("cursor").cloned(),
        with_total: false,
    };
    let page = state
        .resource
        .list(&ctx, &doctype, &spec)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    page_response(&page, &meta, &metas, &site.request_id)
}

pub(super) async fn resource_query(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(doctype): Path<String>,
    request: Request,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    let body = take_body(request, true, &site.request_id).await?;
    let (meta, metas) = prepare_meta(&state, &ctx, &doctype)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let json = body.unwrap_or_else(|| Json::Object(serde_json::Map::new()));
    let spec = parse_query_request(&json, &meta, lookup_over(&metas), &site.request_id)?;
    let page = state
        .resource
        .query(&ctx, &doctype, &spec)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    page_response(&page, &meta, &metas, &site.request_id)
}

/// Validate the contract's `QueryRequest` (`additionalProperties: false`) into a `QuerySpec`.
fn parse_query_request(
    json: &Json,
    meta: &DocTypeMeta,
    meta_of: MetaLookup,
    request_id: &RequestId,
) -> Result<QuerySpec, Failure> {
    let Json::Object(map) = json else {
        return Err(fail(
            ApiError::new(Code::ApiInvalidRequest, "The query body must be an object."),
            request_id,
        ));
    };
    for key in map.keys() {
        if !["fields", "filter", "order", "limit", "cursor", "with_total"].contains(&key.as_str()) {
            return Err(fail(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    format!("`{key}` is not a query field."),
                ),
                request_id,
            ));
        }
    }
    let mut fields = Vec::new();
    match map.get("fields") {
        None | Some(Json::Null) => {}
        Some(Json::Array(list)) => {
            for item in list {
                let Some(part) = item.as_str() else {
                    return Err(fail(
                        ApiError::new(
                            Code::ApiInvalidRequest,
                            "`fields` must be an array of names.",
                        ),
                        request_id,
                    ));
                };
                if meta.field(part).is_none() && part != "id" && part != "version" {
                    return Err(fail(unknown_field_error(meta, part), request_id));
                }
                fields.push(checked_name(part, request_id)?);
            }
        }
        Some(_) => {
            return Err(fail(
                ApiError::new(Code::ApiInvalidRequest, "`fields` must be an array."),
                request_id,
            ));
        }
    }
    let ast = match map.get("filter") {
        None | Some(Json::Null) => None,
        Some(f) => Some(filter::parse_filter(f, meta, meta_of).map_err(|e| fail(e, request_id))?),
    };
    let order = match map.get("order") {
        None | Some(Json::Null) => Vec::new(),
        Some(o) => filter::parse_order(o, meta).map_err(|e| fail(e, request_id))?,
    };
    let limit = match map.get("limit") {
        None | Some(Json::Null) => parse_limit(None, request_id)?,
        Some(v) => {
            let Some(n) = v.as_i64() else {
                return Err(fail(
                    ApiError::new(Code::ApiInvalidRequest, "`limit` must be an integer."),
                    request_id,
                ));
            };
            parse_limit(Some(n), request_id)?
        }
    };
    let cursor = match map.get("cursor") {
        None | Some(Json::Null) => None,
        Some(Json::String(c)) => Some(c.clone()),
        Some(_) => {
            return Err(fail(
                ApiError::new(Code::ApiInvalidRequest, "`cursor` must be a string."),
                request_id,
            ));
        }
    };
    let with_total = match map.get("with_total") {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => {
            return Err(fail(
                ApiError::new(Code::ApiInvalidRequest, "`with_total` must be a boolean."),
                request_id,
            ));
        }
    };
    Ok(QuerySpec {
        fields,
        filter: ast,
        order,
        limit,
        cursor,
        with_total,
    })
}
