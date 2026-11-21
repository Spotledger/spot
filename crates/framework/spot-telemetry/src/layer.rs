//! The tower layer: request span, cost scope, one access-log event and HTTP metrics.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use http::{Request, Response};
use pin_project_lite::pin_project;
use spot_core::SiteId;
use tokio::task::futures::TaskLocalFuture;
use tower::{Layer, Service};
use tracing::Span;

use crate::cost::{self, CostCell};
use crate::metrics;

/// The matched route template (`/api/v1/invoices/{id}`), inserted into the request extensions
/// by the caller. It is a metric label, so it must come from a fixed set: never a raw path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteTemplate(pub &'static str);

/// The site a request is for, inserted into the request extensions by the caller. It goes in
/// the span and the access log, never in a metric label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteField(pub SiteId);

/// Route label used when the caller inserted no [`RouteTemplate`].
pub const UNMATCHED_ROUTE: &str = "unmatched";

/// Status recorded when the response future is dropped before it finishes (the client went
/// away), following the common 499 convention.
pub const CANCELLED_STATUS: u16 = 499;

/// Wraps a service with request tracing, cost accounting, access logging and HTTP metrics.
#[derive(Clone, Copy, Debug, Default)]
pub struct TelemetryLayer;

impl TelemetryLayer {
    /// Creates the layer and makes sure the Prometheus recorder is installed.
    pub fn new() -> Self {
        // Best effort: if another recorder owns the process, requests are still logged.
        let _ = metrics::install();
        Self
    }
}

impl<S> Layer<S> for TelemetryLayer {
    type Service = TelemetryService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        TelemetryService { inner }
    }
}

/// The service [`TelemetryLayer`] builds.
#[derive(Clone, Debug)]
pub struct TelemetryService<S> {
    inner: S,
}

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(1);

/// Takes `x-request-id` if it is short printable ASCII, else makes one from the clock and a
/// process counter.
fn request_id(headers: &http::HeaderMap) -> String {
    if let Some(v) = headers.get("x-request-id").and_then(|v| v.to_str().ok())
        && !v.is_empty()
        && v.len() <= 128
        && v.bytes().all(|b| b.is_ascii_graphic())
    {
        return v.to_owned();
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{nanos:x}-{:x}",
        REQUEST_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// The trace id of a W3C `traceparent` header (`00-<32 hex>-<16 hex>-<2 hex>`), if valid.
fn trace_id(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get("traceparent")?.to_str().ok()?;
    let mut parts = value.split('-');
    let _version = parts.next()?;
    let id = parts.next()?;
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_ascii_lowercase())
}

/// Emits the access log and metrics exactly once: on `finish`, or on drop if never finished.
struct AccessGuard {
    span: Span,
    cell: Arc<CostCell>,
    start: Instant,
    method: &'static str,
    route: &'static str,
    done: bool,
}

impl AccessGuard {
    fn finish(&mut self, status: u16) {
        if self.done {
            return;
        }
        self.done = true;
        let elapsed = self.start.elapsed();
        let cost = self.cell.snapshot(elapsed);
        self.span.record("status", status);
        let _entered = self.span.enter();
        tracing::info!(
            target: "spot_telemetry::access",
            status,
            duration_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            db_statements = cost.db_statements,
            db_time_us = u64::try_from(cost.db_time.as_micros()).unwrap_or(u64::MAX),
            rows_read = cost.rows_read,
            rows_written = cost.rows_written,
            cache_hits = cost.cache_hits,
            cache_misses = cost.cache_misses,
            outbox_events = cost.outbox_events,
            external_calls = cost.external_calls,
            "request"
        );
        metrics::record_http_request(self.method, self.route, status, elapsed);
    }
}

impl Drop for AccessGuard {
    fn drop(&mut self) {
        self.finish(CANCELLED_STATUS);
    }
}

pin_project! {
    /// The response future of [`TelemetryService`].
    pub struct TelemetryFuture<F> {
        #[pin]
        inner: TaskLocalFuture<Arc<CostCell>, F>,
        guard: AccessGuard,
    }
}

impl<F, ResB, E> Future for TelemetryFuture<F>
where
    F: Future<Output = Result<Response<ResB>, E>>,
{
    type Output = Result<Response<ResB>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let span = this.guard.span.clone();
        let _entered = span.enter();
        let mut inner = this.inner;
        match catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(result)) => {
                let status = match &result {
                    Ok(response) => response.status().as_u16(),
                    Err(_) => 500,
                };
                this.guard.finish(status);
                Poll::Ready(result)
            }
            Err(payload) => {
                // The body type is generic, so we cannot build a 500 response here; log and
                // count it as one, then let the panic continue to the server's own handling.
                this.guard.finish(500);
                resume_unwind(payload)
            }
        }
    }
}

impl<S, B, ResB> Service<Request<B>> for TelemetryService<S>
where
    S: Service<Request<B>, Response = Response<ResB>>,
{
    type Response = Response<ResB>;
    type Error = S::Error;
    type Future = TelemetryFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let method = metrics::method_label(req.method());
        let route = req
            .extensions()
            .get::<RouteTemplate>()
            .map_or(UNMATCHED_ROUTE, |r| r.0);
        let site = req
            .extensions()
            .get::<SiteField>()
            .map(|s| s.0.as_str().to_owned());
        let request_id = request_id(req.headers());
        let trace_id = trace_id(req.headers());
        let span = tracing::info_span!(
            "http_request",
            method,
            route,
            status = tracing::field::Empty,
            request_id = %request_id,
            site = site.as_deref(),
            trace_id = trace_id.as_deref(),
        );
        let cell = Arc::new(CostCell::default());
        let guard = AccessGuard {
            span: span.clone(),
            cell: Arc::clone(&cell),
            start: Instant::now(),
            method,
            route,
            done: false,
        };
        // Create the inner future inside the span, so anything it logs on construction counts.
        let fut = span.in_scope(|| self.inner.call(req));
        TelemetryFuture {
            inner: cost::run_in(cell, fut),
            guard,
        }
    }
}
