//! `journal_info_get`, `journal_manifest_get` and `journal_segment_get` (contract 0.16.0,
//! WP-JRN-09.T4, ADR-061 section 7 surface 2): the journal access surface.
//!
//! **The segment route is the whole tenant, unfiltered, by design.** The controls are the
//! grant and the audit record of every call, and both live behind [`JournalAccessPort`]: the
//! routes decide none of them. What the routes do:
//!
//! - take the path as raw text and hand it to the port, which parses it as canonical decimal
//!   numbers and audits even a hostile path (`..`, an encoded separator, a padded number): the
//!   route never builds a key from it, and a `?` query or an unexpected header is ignored;
//! - answer `Cache-Control: private, no-store` on every response, errors included;
//! - stream a segment from the port in bounded chunks with `Content-Length` set, so a stream the
//!   port aborts (a hash mismatch, an audit failure at the end) is a connection that closes short,
//!   never a corrupted `200` that looks complete; `ETag` is the recorded hash;
//! - answer `501 service.not_configured` when the group's port is absent from the route state.
//!
//! The segment path is routed as `/segments/{generation}/{range}` and the port splits `range`
//! at its single `-`: the router's path syntax has no mixed `{a}-{b}` segment, and the contract's
//! URL space is the same.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hyper::body::{Body as HttpBody, Frame};
use serde_json::{Value as Json, json};

use super::{Registration, RouteState};
use crate::api::files::BoxByteStream;
use crate::api::ports::PortKey;
use crate::api::ports::journal_access::{
    JournalAccessError, JournalAccessPort, SegmentDownload, SegmentStatus,
};
use crate::api::registry::Code;
use crate::api::service::ApiError;
use crate::api::storage_error::RETRY_AFTER_DETAIL;
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{build_ctx, render_error};

/// The operation ids this group serves.
pub const SERVES: &[&str] = &[
    "journal_info_get",
    "journal_manifest_get",
    "journal_segment_get",
];

/// The ports this group needs. One port: its adapter holds the grant store, the permission
/// check and the audit, and is provided only when all three exist, so a site without them is
/// not served (fail closed).
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn JournalAccessPort>()];
    &KEYS
}

/// The journal routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/journal", get(journal_info_get))
        .route(
            "/api/v1/journal/manifest/{generation}",
            get(journal_manifest_get),
        )
        .route(
            "/api/v1/journal/segments/{generation}/{range}",
            get(journal_segment_get),
        )
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "journal_access",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn error_response(error: ApiError, request_id: &RequestId) -> Response {
    no_store(render_error(error, request_id))
}

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no journal access configured.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    no_store(response)
}

/// Maps a port refusal. A denial is one `403`, with `details.reason` only for an expired grant;
/// the body never says whether a grant once existed or which check failed.
fn api_error(error: JournalAccessError) -> ApiError {
    match error {
        JournalAccessError::Unauthenticated => ApiError::from_code(Code::AuthSessionExpired),
        JournalAccessError::Denied { expired: false } => {
            ApiError::from_code(Code::PermissionDenied)
        }
        JournalAccessError::Denied { expired: true } => ApiError::from_code(Code::PermissionDenied)
            .detail("reason", Json::String("grant_expired".to_owned())),
        JournalAccessError::NotFound => ApiError::from_code(Code::JournalRangeUnavailable),
        JournalAccessError::RateLimited { retry_after_secs } => {
            ApiError::from_code(Code::ApiRateLimited)
                .detail(RETRY_AFTER_DETAIL, Json::from(retry_after_secs.max(1)))
        }
        JournalAccessError::Unavailable => ApiError::from_code(Code::ServerUnavailable),
    }
}

async fn journal_info_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
) -> Response {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn JournalAccessPort>() else {
        return not_configured(request_id);
    };
    let ctx = build_ctx(&site, &headers, None);
    if ctx.credential.is_none() {
        return error_response(ApiError::from_code(Code::AuthRequired), request_id);
    }
    match port.info(&ctx).await {
        Ok(info) => no_store(
            (
                StatusCode::OK,
                axum::Json(json!({ "data": info.to_json() })),
            )
                .into_response(),
        ),
        Err(error) => error_response(api_error(error), request_id),
    }
}

async fn journal_manifest_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(generation): Path<String>,
) -> Response {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn JournalAccessPort>() else {
        return not_configured(request_id);
    };
    let ctx = build_ctx(&site, &headers, None);
    if ctx.credential.is_none() {
        return error_response(ApiError::from_code(Code::AuthRequired), request_id);
    }
    match port.manifest(&ctx, &generation).await {
        Ok(manifest) => no_store((StatusCode::OK, axum::Json(manifest)).into_response()),
        Err(error) => error_response(api_error(error), request_id),
    }
}

/// A segment body pulled from the port chunk by chunk. A failure mid-stream aborts the response
/// (the head is already sent) with a message free of keys and content.
struct PortBody {
    stream: BoxByteStream,
}

impl HttpBody for PortBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        loop {
            match self.stream.as_mut().poll_chunk(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Ok(chunk))) if chunk.is_empty() => {}
                Poll::Ready(Some(Ok(chunk))) => return Poll::Ready(Some(Ok(Frame::data(chunk)))),
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(std::io::Error::other(error.message))));
                }
            }
        }
    }
}

fn segment_response(download: SegmentDownload) -> Response {
    let status = match download.status {
        SegmentStatus::Full => StatusCode::OK,
        SegmentStatus::Partial => StatusCode::PARTIAL_CONTENT,
        SegmentStatus::RangeNotSatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
    };
    let body = match (download.status, download.body) {
        (SegmentStatus::RangeNotSatisfiable, _) | (_, None) => Body::empty(),
        (_, Some(stream)) => Body::new(PortBody { stream }),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    out.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    out.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(download.content_length),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("\"{}\"", download.etag)) {
        out.insert(header::ETAG, value);
    }
    if let Some(range) = &download.content_range
        && let Ok(value) = HeaderValue::from_str(range)
    {
        out.insert(header::CONTENT_RANGE, value);
    }
    no_store(response)
}

async fn journal_segment_get(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((generation, range)): Path<(String, String)>,
) -> Response {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn JournalAccessPort>() else {
        return not_configured(request_id);
    };
    let ctx = build_ctx(&site, &headers, None);
    if ctx.credential.is_none() {
        return error_response(ApiError::from_code(Code::AuthRequired), request_id);
    }
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= 100)
        .map(str::to_owned);
    match port
        .segment(&ctx, &generation, &range, range_header.as_deref())
        .await
    {
        Ok(download) => segment_response(download),
        Err(error) => error_response(api_error(error), request_id),
    }
}
