//! Document status actions, method and function calls, reports: the write-shaped
//! operations that go through idempotency, plus the read-shaped `GET` function call.

use std::collections::BTreeMap;

use crate::api::codec::encode_document;
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;
use axum::extract::{Path, Query, Request};
use axum::http::{HeaderMap, StatusCode};
use serde_json::Value as Json;

use super::{
    Failure, HandlerResult, SharedState, build_ctx, document_response, envelope, fail,
    json_response, lookup_over, parse_if_match, path_identifier, path_record_id, prepare_meta,
    stamp_etag, take_body, validated_key,
};
#[derive(Clone, Copy, PartialEq, Eq)]
enum Transition {
    Post,
    Cancel,
    Correct,
}

async fn lifecycle_transition(
    state: &SharedState,
    site: &SiteContext,
    headers: &HeaderMap,
    doctype: &str,
    id: &str,
    kind: Transition,
    key: Option<String>,
) -> HandlerResult {
    path_identifier(doctype, "DocType", &site.request_id)?;
    let id = path_record_id(id, &site.request_id)?;
    let expected = parse_if_match(headers, true, &site.request_id)?.ok_or_else(|| {
        fail(
            ApiError::from_code(Code::ApiPreconditionRequired),
            &site.request_id,
        )
    })?;
    let ctx = build_ctx(site, headers, key);
    let result = match kind {
        Transition::Post => state.resource.post(&ctx, doctype, id, expected).await,
        Transition::Cancel => state.resource.cancel(&ctx, doctype, id, expected).await,
        Transition::Correct => state.resource.correct(&ctx, doctype, id, expected).await,
    }
    .map_err(|e| fail(e, &site.request_id))?;
    let (meta, metas) = prepare_meta(state, &ctx, doctype)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let status = if kind == Transition::Correct {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    document_response(
        status,
        &result,
        &meta,
        &metas,
        // `correct` creates the amended draft, so it carries `Location` too.
        if kind == Transition::Correct {
            Some(doctype)
        } else {
            None
        },
        &site.request_id,
    )
}

pub(super) async fn lifecycle_post(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id)): Path<(String, String)>,
    request: Request,
) -> HandlerResult {
    lifecycle_transition(
        &state,
        &site,
        &headers,
        &doctype,
        &id,
        Transition::Post,
        validated_key(&request),
    )
    .await
}

pub(super) async fn lifecycle_cancel(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id)): Path<(String, String)>,
    request: Request,
) -> HandlerResult {
    lifecycle_transition(
        &state,
        &site,
        &headers,
        &doctype,
        &id,
        Transition::Cancel,
        validated_key(&request),
    )
    .await
}

pub(super) async fn lifecycle_correct(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id)): Path<(String, String)>,
    request: Request,
) -> HandlerResult {
    lifecycle_transition(
        &state,
        &site,
        &headers,
        &doctype,
        &id,
        Transition::Correct,
        validated_key(&request),
    )
    .await
}

pub(super) async fn lifecycle_rename(
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
    let ctx = build_ctx(&site, &headers, validated_key(&request));
    let body = take_body(request, true, &site.request_id).await?;
    let json = body.unwrap_or(Json::Null);
    let new_name = rename_name(&json, &site.request_id)?;
    let result = state
        .resource
        .rename(&ctx, &doctype, id, &new_name, expected)
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

fn rename_name(body: &Json, request_id: &RequestId) -> Result<String, Failure> {
    let Json::Object(map) = body else {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "The rename body must be an object.",
            ),
            request_id,
        ));
    };
    if map.keys().any(|k| k != "name") {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "The rename body takes only the `name` field.",
            ),
            request_id,
        ));
    }
    let Some(name) = map.get("name").and_then(Json::as_str) else {
        return Err(fail(
            ApiError::new(Code::ApiInvalidRequest, "`name` is required."),
            request_id,
        ));
    };
    if name.is_empty() || name.chars().count() > 140 {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "`name` must be 1 to 140 characters.",
            ),
            request_id,
        ));
    }
    Ok(name.to_owned())
}

// ---------------------------------------------------------------------------
// Methods and functions

pub(super) async fn document_method_call(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id, method)): Path<(String, String, String)>,
    request: Request,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    path_identifier(&method, "method", &site.request_id)?;
    let id = path_record_id(&id, &site.request_id)?;
    // The contract keeps `If-Match` optional here: a method that mutates declares it and the
    // service answers `428 api.precondition_required` when it is missing.
    let expected = parse_if_match(&headers, false, &site.request_id)?;
    let ctx = build_ctx(&site, &headers, validated_key(&request));
    let body = take_body(request, false, &site.request_id).await?;
    let args = method_args(body.as_ref(), &site.request_id)?;
    let (result, messages) = state
        .resource
        .call_method(&ctx, &doctype, id, &method, args, expected)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let (meta, metas) = prepare_meta(&state, &ctx, &doctype)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let data_doc = encode_document(&result.document, &meta, None, lookup_over(&metas))
        .map_err(|e| fail(e, &site.request_id))?;
    let mut data = serde_json::Map::new();
    data.insert("document".to_owned(), data_doc);
    data.insert("result".to_owned(), result.result);
    let mut response = json_response(StatusCode::OK, envelope(Json::Object(data), &messages));
    stamp_etag(&mut response, &result.document);
    Ok(response)
}

fn method_args(body: Option<&Json>, request_id: &RequestId) -> Result<Json, Failure> {
    match body {
        None | Some(Json::Null) => Ok(Json::Object(serde_json::Map::new())),
        Some(Json::Object(map)) => {
            for key in map.keys() {
                if key != "args" {
                    return Err(fail(
                        ApiError::new(
                            Code::ApiInvalidRequest,
                            "`args` is the only method-call field.",
                        ),
                        request_id,
                    ));
                }
            }
            match map.get("args") {
                None | Some(Json::Null) => Ok(Json::Object(serde_json::Map::new())),
                Some(Json::Object(args)) => Ok(Json::Object(args.clone())),
                Some(_) => Err(fail(
                    ApiError::new(Code::ApiInvalidRequest, "`args` must be an object."),
                    request_id,
                )),
            }
        }
        Some(_) => Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "The method-call body must be an object.",
            ),
            request_id,
        )),
    }
}

pub(super) async fn function_call_post(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((app, module, function)): Path<(String, String, String)>,
    request: Request,
) -> HandlerResult {
    let path = FunctionPath {
        app: &app,
        module: &module,
        function: &function,
    };
    path.validate(&site.request_id)?;
    let ctx = build_ctx(&site, &headers, validated_key(&request));
    let body = take_body(request, false, &site.request_id).await?;
    let args = method_args(body.as_ref(), &site.request_id)?;
    call_function(&state, &ctx, path, args, false, &site.request_id).await
}

pub(super) async fn function_call_get(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((app, module, function)): Path<(String, String, String)>,
    Query(pairs): Query<BTreeMap<String, String>>,
) -> HandlerResult {
    let path = FunctionPath {
        app: &app,
        module: &module,
        function: &function,
    };
    path.validate(&site.request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    // GET functions take arguments from the query string; every value arrives as a string the
    // service decodes per the function's declared parameter types.
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
        map.insert(k, Json::String(v));
    }
    call_function(
        &state,
        &ctx,
        path,
        Json::Object(map),
        true,
        &site.request_id,
    )
    .await
}

/// One function's path name, split at its three segments.
struct FunctionPath<'a> {
    app: &'a str,
    module: &'a str,
    function: &'a str,
}

impl FunctionPath<'_> {
    fn validate(&self, request_id: &RequestId) -> Result<(), Failure> {
        for (value, what) in [
            (self.app, "app"),
            (self.module, "module"),
            (self.function, "function"),
        ] {
            path_identifier(value, what, request_id)?;
        }
        Ok(())
    }
}

async fn call_function(
    state: &SharedState,
    ctx: &RequestCtx,
    path: FunctionPath<'_>,
    args: Json,
    via_get: bool,
    request_id: &RequestId,
) -> HandlerResult {
    let (result, messages) = state
        .resource
        .call_function(ctx, path.app, path.module, path.function, args, via_get)
        .await
        .map_err(|e| fail(e, request_id))?;
    Ok(json_response(StatusCode::OK, envelope(result, &messages)))
}

// ---------------------------------------------------------------------------
// Reports

pub(super) async fn report_run(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(report): Path<String>,
    request: Request,
) -> HandlerResult {
    path_identifier(&report, "report", &site.request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    let body = take_body(request, false, &site.request_id).await?;
    let mut filters = Json::Object(serde_json::Map::new());
    let mut limit = None;
    match body {
        None | Some(Json::Null) => {}
        Some(Json::Object(map)) => {
            for key in map.keys() {
                if key != "filters" && key != "limit" {
                    return Err(fail(
                        ApiError::new(
                            Code::ApiInvalidRequest,
                            format!("`{key}` is not a report-run field."),
                        ),
                        &site.request_id,
                    ));
                }
            }
            if let Some(f) = map.get("filters") {
                match f {
                    Json::Object(_) => filters = f.clone(),
                    _ => {
                        return Err(fail(
                            ApiError::new(Code::ApiInvalidRequest, "`filters` must be an object."),
                            &site.request_id,
                        ));
                    }
                }
            }
            if let Some(v) = map.get("limit") {
                let Some(n) = v.as_i64() else {
                    return Err(fail(
                        ApiError::new(Code::ApiInvalidRequest, "`limit` must be an integer."),
                        &site.request_id,
                    ));
                };
                if !(1..=100_000).contains(&n) {
                    return Err(fail(
                        ApiError::new(
                            Code::ApiInvalidRequest,
                            "`limit` must be between 1 and 100000.",
                        ),
                        &site.request_id,
                    ));
                }
                limit = Some(n as u64);
            }
        }
        Some(_) => {
            return Err(fail(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    "The report body must be an object.",
                ),
                &site.request_id,
            ));
        }
    }
    let (result, messages) = state
        .resource
        .run_report(&ctx, &report, filters, limit)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let data = report_json(&result);
    Ok(json_response(StatusCode::OK, envelope(data, &messages)))
}

fn report_json(result: &crate::api::service::ReportResult) -> Json {
    let mut map = serde_json::Map::new();
    let columns: Vec<Json> = result
        .columns
        .iter()
        .map(|c| {
            let mut m = serde_json::Map::new();
            m.insert("name".to_owned(), Json::String(c.name.clone()));
            m.insert("label".to_owned(), Json::String(c.label.clone()));
            m.insert(
                "type".to_owned(),
                Json::String(c.field_type.as_str().to_owned()),
            );
            Json::Object(m)
        })
        .collect();
    map.insert("columns".to_owned(), Json::Array(columns));
    map.insert(
        "rows".to_owned(),
        Json::Array(result.rows.iter().cloned().map(Json::Array).collect()),
    );
    map.insert("truncated".to_owned(), Json::Bool(result.truncated));
    Json::Object(map)
}
