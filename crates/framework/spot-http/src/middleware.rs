//! Cross-cutting middleware: request spans, panic interception, timeouts, the body limit
//! (declared content-length and streamed/chunked bodies alike).

use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use spot_telemetry::{RouteTemplate, SiteField, UNMATCHED_ROUTE};
use tracing::Instrument;

use crate::config::ServerLimits;
use crate::error::{HttpErrorCode, error_response};
use crate::request::{REQUEST_ID_WIRE, RequestId};
use crate::site::SiteContext;

/// Outermost span: every request gets a span with method, route template and request id (never the concrete path: an open route may carry a secret in it); the response
/// status is recorded when the handler returns. Tracing stays optional: with no subscriber
/// installed, the span is a cheap no-op (WP-PLT-01, not WP-PLT-15).
pub(crate) async fn span_middleware(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let span = tracing::info_span!(
        "spot_http_request",
        method = %method,
        site = tracing::field::Empty,
        route = tracing::field::Empty,
        request_id = %request_id,
        status = tracing::field::Empty,
    );
    let response = next.run(request).instrument(span.clone()).await;
    span.record("status", u64::from(response.status().as_u16()));
    response
}

/// Fills in what [`TelemetryLayer`](spot_telemetry::TelemetryLayer) reads before it builds the
/// request span: the matched route template, the site, and the request id header.
///
/// It runs *inside* site resolution and *outside* the telemetry layer, so both extensions exist
/// by the time the layer calls the inner service. The route comes from axum's [`MatchedPath`]
/// (the pattern the router matched, never the raw path, which carries ids); a request that
/// matched no route gets [`UNMATCHED_ROUTE`]. `request_id_middleware` keeps the id in the
/// request extensions only, so it is echoed into the header here and the access log, the
/// response header and the panic log all carry the same id.
pub(crate) async fn telemetry_fields_middleware(mut request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or(UNMATCHED_ROUTE, |matched| intern_route(matched.as_str()));
    request.extensions_mut().insert(RouteTemplate(route));
    if let Some(site) = request
        .extensions()
        .get::<SiteContext>()
        .map(|context| context.site.clone())
    {
        request.extensions_mut().insert(SiteField(site));
    }
    if let Some(id) = request.extensions().get::<RequestId>()
        && let Ok(value) = axum::http::HeaderValue::from_str(id.as_str())
    {
        request.headers_mut().insert(REQUEST_ID_WIRE, value);
    }
    // The existing span carries the template too; it is already declared and entered here.
    tracing::Span::current().record("route", route);
    next.run(request).await
}

/// Interns a route template so it can be a `&'static str` metric label.
///
/// [`RouteTemplate`] is `&'static str` because it must come from a fixed set, but axum owns the
/// matched path. Routes are fixed when the router is built, so each distinct template is leaked
/// exactly once and reused; the set is bounded by the number of routes, not by requests. Only
/// the first request for a template allocates.
fn intern_route(path: &str) -> &'static str {
    static ROUTES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let routes = ROUTES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut routes = routes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = routes.get(path) {
        return existing;
    }
    let leaked: &'static str = Box::leak(path.to_owned().into_boxed_str());
    routes.insert(leaked);
    leaked
}

/// Wraps the panic interception: a panicked handler becomes the `http.internal` envelope. The
/// panic text stays out of the response (log only, level `error`).
pub(crate) async fn catch_panic_middleware(request: Request, next: Next) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let next = Box::pin(next.run(request));
    match PanicCatch::new(next).await {
        Ok(response) => response,
        Err(panic) => {
            tracing::error!(
                request_id = %request_id,
                panic = %panic,
                "request handler panicked",
            );
            error_response(
                HttpErrorCode::Internal,
                HttpErrorCode::Internal.default_message(),
                Some(&request_id),
            )
        }
    }
}

/// The payload of a caught panic, `Debug`-formatted for logging.
struct PanicPayload(String);

impl std::fmt::Display for PanicPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        String::from("panic payload of unexpected type")
    }
}

/// Future wrapper reporting panics through a `Result`.
struct PanicCatch<F> {
    /// The wrapped inner future.
    inner: Pin<Box<F>>,
}

impl<F: Future> PanicCatch<F> {
    fn new(inner: Pin<Box<F>>) -> Self {
        PanicCatch { inner }
    }
}

impl<F: Future> Future for PanicCatch<F> {
    type Output = Result<F::Output, PanicPayload>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| this.inner.as_mut().poll(cx))) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(PanicPayload(payload_text(payload.as_ref())))),
        }
    }
}

/// Whole-request timeout: a request that does not finish in time gets the `http.timeout`
/// envelope and its connection is dropped when its response completes.
pub(crate) async fn timeout_middleware(
    State(limits): State<Arc<ServerLimits>>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    match tokio::time::timeout(limits.request_timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(
                request_id = %request_id,
                budget_secs = limits.request_timeout.as_secs(),
                "request timed out",
            );
            error_response(
                HttpErrorCode::Timeout,
                HttpErrorCode::Timeout.default_message(),
                Some(&request_id),
            )
        }
    }
}

/// Body rejection before the handler does work: an oversized declared `Content-Length` is
/// refused outright, and any other body (`chunked`, no length) is capped on the body stream so
/// exceeding it still produces the `http.body_too_large` envelope.
pub(crate) async fn body_limit_middleware(
    State(limits): State<Arc<ServerLimits>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(length) = content_length(&request)
        && length > limits.max_body_bytes
    {
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .cloned()
            .unwrap_or_else(RequestId::generate);
        tracing::warn!(
            request_id = %request_id,
            content_length = length,
            limit = limits.max_body_bytes,
            "request body exceeds the content-length limit",
        );
        return error_response(
            HttpErrorCode::BodyTooLarge,
            HttpErrorCode::BodyTooLarge.default_message(),
            Some(&request_id),
        );
    }
    // No declared length (chunked or empty): cap the stream itself. `Limited` errors the moment
    // more than `max_body_bytes` bytes have been produced, whether or not the handler consumes
    // the body; the error branch below maps that to the envelope.
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let max = usize::try_from(limits.max_body_bytes).unwrap_or(usize::MAX);
    let (parts, body) = request.into_parts();
    // `Limited` errors past the cap; wrapping it back into an axum `Body` keeps the signature
    // `Next` expects while the handler still sees a streaming body.
    let body = axum::body::Body::new(http_body_util::Limited::new(body, max));
    let response = next.run(Request::from_parts(parts, body)).await;
    if response.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
        tracing::warn!(
            request_id = %request_id,
            limit = limits.max_body_bytes,
            "request body exceeded the streaming limit",
        );
        return error_response(
            HttpErrorCode::BodyTooLarge,
            HttpErrorCode::BodyTooLarge.default_message(),
            Some(&request_id),
        );
    }
    response
}

/// The declared content length, when it is a plain number.
fn content_length<B>(request: &axum::http::Request<B>) -> Option<u64> {
    let raw = request.headers().get("content-length")?.to_str().ok()?;
    if raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Must;

    #[test]
    fn payload_text_reads_str_and_string() {
        let as_str: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(payload_text(as_str.as_ref()), "boom");
        let as_string: Box<dyn std::any::Any + Send> = Box::new(String::from("bang"));
        assert_eq!(payload_text(as_string.as_ref()), "bang");
        let other: Box<dyn std::any::Any + Send> = Box::new(7u32);
        assert_eq!(
            payload_text(other.as_ref()),
            "panic payload of unexpected type"
        );
    }

    #[test]
    fn content_length_reads_plain_numbers() {
        let request = Request::builder()
            .header("content-length", "2048")
            .body(())
            .must();
        assert_eq!(content_length(&request), Some(2048));
    }

    #[test]
    fn content_length_ignores_garbage() {
        let request = Request::builder()
            .header("content-length", "abc")
            .body(())
            .must();
        assert_eq!(content_length(&request), None);
    }
}
