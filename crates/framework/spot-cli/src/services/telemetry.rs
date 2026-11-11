//! Metrics and request observability of one site.
//!
//! `spot-telemetry` owns the recorder, the access-log layer and the OTLP exporter; this module
//! only mounts what it renders:
//!
//! - `GET /metrics` in the Prometheus text format, behind the same credential as `/api/v1`
//!   (a request without one is refused before it reaches the engine) and only to an
//!   administrator of the site the request resolved to, or open on the loopback
//!   when `services.metrics.require_auth = false` — a scrape from a co-located collector;
//! - the [`TelemetryLayer`] on the site's router, which gives every request an id, a span, one
//!   access-log line and the cost counters.
//!
//! The exporter itself is installed once per process by `spot`'s `main` from
//! `common.toml` `[telemetry]`, before any site exists.

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_http::request::RequestId;

use crate::services::{CheckFuture, Level, RunFuture, Service};

/// The Prometheus text exposition content type.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `GET /metrics`: every counter and histogram the process has recorded.
pub async fn metrics() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        spot_telemetry::render_prometheus(),
    )
        .into_response()
}

/// The host's statement that the authenticated caller of this request holds the administrator
/// permission on the site the request resolved to. The host inserts it into the request
/// extensions after it has authenticated and checked the caller; a client cannot supply it
/// (the extensions are server-side and are rebuilt for every request).
#[derive(Clone, Copy, Debug)]
pub struct MetricsAccess;

/// The refusal an unauthorised API call gets: 403, `permission.denied`, the standard problem
/// body. It names no permission and carries no metric text, and is the same for a caller with
/// some rights and a caller with none.
pub fn refusal(request_id: &str) -> Response {
    let body = ApiError::from_code(Code::PermissionDenied).to_envelope_json(request_id);
    (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
}

/// `GET /metrics` behind the host's check: without [`MetricsAccess`] on the request it is
/// refused, so a route that is reached by any other path never prints the counters.
async fn scrape(request: Request) -> Response {
    if request.extensions().get::<MetricsAccess>().is_none() {
        let id = request
            .extensions()
            .get::<RequestId>()
            .map(|id| id.as_str().to_owned())
            .unwrap_or_default();
        return refusal(&id);
    }
    metrics().await
}

/// The metrics route, merged into the site's authenticated router.
pub fn router() -> Router {
    Router::new().route("/metrics", get(scrape))
}

/// The metrics service: its check proves the recorder renders, its task only waits.
pub struct Telemetry {
    enabled: bool,
    require_auth: bool,
}

impl Telemetry {
    /// A service for the configured metrics options.
    pub fn new(enabled: bool, require_auth: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            require_auth,
        })
    }

    /// Whether `/metrics` is served at all.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a scrape needs the site credential.
    pub fn require_auth(&self) -> bool {
        self.require_auth
    }
}

impl Service for Telemetry {
    fn name(&self) -> &'static str {
        "telemetry"
    }

    fn level(&self) -> Level {
        // Losing metrics must never take a site's API down.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if !self.enabled {
                return Err("metrics are disabled in common.toml".into());
            }
            // Installing is idempotent and is the only way to learn the recorder is usable:
            // an empty rendering is normal before the first request is counted.
            spot_telemetry::metrics::install().map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn run(&self, stop: tokio::sync::watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            // Nothing to drive: the request layer records as requests arrive.
            let _ = stop.changed().await;
            Ok(())
        })
    }
}
