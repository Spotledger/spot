//! The handlers of the store API contract (`contracts/store-api`), plus `/health`.
//!
//! A handler does only what must run on the async side: rate limits, credential parsing, size and
//! content-type checks before any body is read, and building the response. Everything that calls
//! the registry, the index or the state store is a closure sent to the worker pool (plain OS
//! threads, see `pool`).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;

use crate::auth::{ReqCtx, bearer, optional_publisher, require_publisher};
use crate::error::{ApiError, Wire};
use crate::http::{
    Slice, data_response, idempotency_key, if_none_match, json_response, limit, parse_app,
    parse_range, parse_version, read_json, set_header, unsatisfiable_range,
};
use crate::ops::{
    SearchParams, app_sync, listing_json, read_bundle, resolve_download, review_sync, search_sync,
    version_json, version_sync, yank_sync,
};
use crate::publish::{publish_sync, read_upload};
use crate::ratelimit::Class;
use crate::service::Service;

/// The slack added to the bundle cap for the multipart framing and the small parts.
pub(crate) const MULTIPART_OVERHEAD: u64 = 1024 * 1024;

/// The query string of `app_search`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchQueryParams {
    query: Option<String>,
    category: Option<String>,
    capability: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

/// The body of `app_version_yank`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YankBody {
    reason: String,
}

fn capability_ok(capability: &str) -> bool {
    match capability.split_once('.') {
        Some(("doctypes", "read" | "write")) => true,
        Some(_) => false,
        None => matches!(capability, "network" | "email" | "files"),
    }
}

/// `app_search`.
pub(crate) async fn app_search(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    query: Result<Query<SearchQueryParams>, QueryRejection>,
) -> Result<Response, ApiError> {
    limit(&service, Class::Public, &ctx.addr)?;
    let Query(params) = query.map_err(|_| ApiError::invalid("the query string is not valid"))?;
    let text = params.query.unwrap_or_default();
    if text.chars().count() > 256 {
        return Err(ApiError::invalid("query is longer than 256 characters"));
    }
    if params
        .category
        .as_deref()
        .is_some_and(|category| category.chars().count() > 64)
    {
        return Err(ApiError::invalid("category is longer than 64 characters"));
    }
    if params
        .capability
        .as_deref()
        .is_some_and(|capability| !capability_ok(capability))
    {
        return Err(ApiError::invalid(
            "capability is not one the contract defines",
        ));
    }
    let page_size = params.limit.unwrap_or(20);
    if !(1..=100).contains(&page_size) {
        return Err(ApiError::invalid("limit must be 1 to 100"));
    }
    if params
        .cursor
        .as_deref()
        .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 2048)
    {
        return Err(ApiError::invalid("cursor must be 1 to 2048 characters"));
    }
    let search = SearchParams {
        text,
        category: params.category,
        capability: params.capability,
        limit: page_size,
        cursor: params.cursor,
    };
    let core = Arc::clone(&service.core);
    let outcome = service
        .pool
        .call(move || search_sync(&core, &search))
        .await??;
    let mut data = Vec::with_capacity(outcome.releases.len());
    for release in &outcome.releases {
        data.push(listing_json(release)?);
    }
    Ok(json_response(
        StatusCode::OK,
        &json!({ "data": data, "next_cursor": outcome.next_cursor }),
    ))
}

/// `app_get`.
pub(crate) async fn app_get(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    limit(&service, Class::Public, &ctx.addr)?;
    let app = parse_app(&name)?;
    let owner = optional_publisher(&service, &ctx.addr, &headers)
        .await?
        .map(|principal| principal.publisher_id);
    let core = Arc::clone(&service.core);
    let release = service
        .pool
        .call(move || app_sync(&core, &app, owner.as_deref()))
        .await??;
    Ok(data_response(StatusCode::OK, listing_json(&release)?))
}

/// `app_version_get`.
pub(crate) async fn app_version_get(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path((name, version)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    limit(&service, Class::Public, &ctx.addr)?;
    let app = parse_app(&name)?;
    let version = parse_version(&version)?;
    let owner = optional_publisher(&service, &ctx.addr, &headers)
        .await?
        .map(|principal| principal.publisher_id);
    let core = Arc::clone(&service.core);
    let release = service
        .pool
        .call(move || version_sync(&core, &app, &version, owner.as_deref()))
        .await??;
    Ok(data_response(StatusCode::OK, version_json(&release)?))
}

/// `app_bundle_download`: the stored archive with its digest as `ETag`, single-range and
/// conditional requests. The response is never a statement that the bytes are verified.
pub(crate) async fn app_bundle_download(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path((name, version)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    limit(&service, Class::Public, &ctx.addr)?;
    let app = parse_app(&name)?;
    let version = parse_version(&version)?;
    let token = bearer(&headers, service.core.config.limits.max_credential_bytes)?;
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    let release = service
        .pool
        .call(move || resolve_download(&core, &app, &version, token.as_deref(), now))
        .await??;
    let etag = format!("\"{}\"", release.digest.to_hex());
    if if_none_match(&headers, &etag) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        set_header(&mut response, "etag", &etag);
        return Ok(response);
    }
    let Ok(slice) = parse_range(&headers, release.size) else {
        return Ok(unsatisfiable_range(release.size));
    };
    let core = Arc::clone(&service.core);
    let stored = release.clone();
    let bytes = service
        .pool
        .call(move || read_bundle(&core, &stored))
        .await??;
    let (status, body, content_range) = match slice {
        Slice::Full => (StatusCode::OK, bytes, None),
        Slice::Part { start, end } => {
            let from = usize::try_from(start).map_err(|_| ApiError::internal())?;
            let to = usize::try_from(end).map_err(|_| ApiError::internal())?;
            let part = bytes
                .get(from..=to)
                .ok_or_else(ApiError::internal)?
                .to_vec();
            let range = format!("bytes {start}-{end}/{}", release.size);
            (StatusCode::PARTIAL_CONTENT, part, Some(range))
        }
    };
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    set_header(&mut response, "content-type", "application/octet-stream");
    set_header(&mut response, "etag", &etag);
    set_header(&mut response, "accept-ranges", "bytes");
    set_header(&mut response, "cache-control", "no-cache");
    if let Some(range) = content_range {
        set_header(&mut response, "content-range", &range);
    }
    // The detached signature and the certificate travel with the bundle (the contract's
    // `signature_id` says so): the site verifies them itself, and neither header says the bytes
    // are verified.
    let proof = &release.signature_ref;
    let encode = |text: String| base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    set_header(
        &mut response,
        "x-spot-signature",
        &encode(proof.signature.to_json()),
    );
    set_header(
        &mut response,
        "x-spot-certificate",
        &encode(proof.chain.leaf.to_json()),
    );
    Ok(response)
}

/// `app_publish`: authenticates, reads the multipart upload within its caps, then hands the
/// blocking steps to a worker. The answer is `202` with the review id; never "listed".
pub(crate) async fn app_publish(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(name): Path<String>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Response, ApiError> {
    limit(&service, Class::PublishAddress, &ctx.addr)?;
    let principal = require_publisher(&service, &ctx.addr, &headers).await?;
    limit(&service, Class::PublishKey, &principal.rate_key)?;
    let app = parse_app(&name)?;
    let key = idempotency_key(&headers)?;
    let limits = service.core.config.limits.clone();
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    if declared.is_some_and(|length| length > limits.max_bundle_bytes + MULTIPART_OVERHEAD) {
        return Err(ApiError::wire(Wire::TooLarge));
    }
    let multipart =
        multipart.map_err(|_| ApiError::invalid("the body must be multipart/form-data"))?;
    let temp_dir = service.core.config.temp_dir.clone();
    let upload = tokio::time::timeout(
        Duration::from_secs(limits.upload_timeout_secs),
        read_upload(multipart, &limits, &temp_dir),
    )
    .await
    .map_err(|_| {
        ApiError::invalid("the upload took too long").with_status(StatusCode::REQUEST_TIMEOUT)
    })??;
    let core = Arc::clone(&service.core);
    let accepted = service
        .pool
        .call(move || publish_sync(&core, &principal, &app, &upload, key.as_deref()))
        .await??;
    let mut response = data_response(
        StatusCode::ACCEPTED,
        json!({ "review_id": accepted.review_id }),
    );
    if accepted.replayed {
        set_header(&mut response, "idempotency-replayed", "true");
    }
    Ok(response)
}

/// `app_version_yank`.
pub(crate) async fn app_version_yank(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path((name, version)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    limit(&service, Class::PublishAddress, &ctx.addr)?;
    let principal = require_publisher(&service, &ctx.addr, &headers).await?;
    limit(&service, Class::PublishKey, &principal.rate_key)?;
    let app = parse_app(&name)?;
    let version = parse_version(&version)?;
    let body: YankBody = read_json(
        &headers,
        request.into_body(),
        service.core.config.limits.max_json_bytes,
        false,
    )
    .await?;
    let reason = body.reason.trim().to_owned();
    if reason.is_empty() || reason.len() > 2000 {
        return Err(ApiError::invalid("reason must be 1 to 2000 bytes"));
    }
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    let release = service
        .pool
        .call(move || yank_sync(&core, &principal, &app, &version, &reason, now))
        .await??;
    crate::metrics::Metrics::bump(&service.core.metrics.yanks);
    Ok(data_response(StatusCode::OK, version_json(&release)?))
}

/// `revocations_get`: the last issued list, the same bytes for every caller; `ETag` is its
/// generation and `Cache-Control` is `max-age=300`.
pub(crate) async fn revocations_get(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    limit(&service, Class::Public, &ctx.addr)?;
    let served = service.core.revocations.served();
    let etag = served.etag();
    let mut response = if if_none_match(&headers, &etag) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response
    } else {
        // `{"data": <list>}`: only the serving envelope is written here; the list is the text
        // `FreshRevocationList::to_wire_json` produced.
        let mut response = Response::new(Body::from(format!("{{\"data\":{}}}", served.body)));
        set_header(&mut response, "content-type", "application/json");
        response
    };
    set_header(&mut response, "etag", &etag);
    set_header(&mut response, "cache-control", "max-age=300");
    Ok(response)
}

/// `review_get`.
pub(crate) async fn review_get(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    limit(&service, Class::Public, &ctx.addr)?;
    let principal = require_publisher(&service, &ctx.addr, &headers).await?;
    limit(&service, Class::Review, &principal.rate_key)?;
    let core = Arc::clone(&service.core);
    let value = service
        .pool
        .call(move || review_sync(&core, &principal, &id))
        .await??;
    Ok(data_response(StatusCode::OK, value))
}

/// `/health`: 200 while the revocation list is fresh and a review worker is free, else 503. It
/// reads memory only, so a database outage does not hide behind it.
pub(crate) async fn health(State(service): State<Arc<Service>>) -> Response {
    let now = service.core.clock.now();
    let stale = service.core.revocations.is_stale(now);
    let accepting = service.core.reviewer.accepting();
    let healthy = !stale && accepting;
    json_response(
        if healthy {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        &json!({
            "status": if healthy { "ok" } else { "degraded" },
            "revocation_list_fresh": !stale,
            "review_workers_accepting": accepting,
        }),
    )
}
