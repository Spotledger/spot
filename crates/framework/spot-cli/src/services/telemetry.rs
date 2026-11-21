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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_http::request::RequestId;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};

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

/// The routes development tools add to a site's router, filed by site name (WP-DX-03.T4).
///
/// A development module files its router here when it registers (it registers only under its own
/// guards, so a production host files nothing); the host merges what is filed for the site with
/// [`DevRoutes::router_for`] when it builds the site's router. A site with nothing filed gets an
/// empty router, so the hook costs a non-dev host one map lookup.
#[derive(Default)]
pub struct DevRoutes {
    routes: Mutex<BTreeMap<String, Router>>,
}

/// The process's route hook for development tools.
#[must_use]
pub fn dev_routes() -> &'static DevRoutes {
    static ROUTES: OnceLock<DevRoutes> = OnceLock::new();
    ROUTES.get_or_init(DevRoutes::default)
}

impl DevRoutes {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Router>> {
        self.routes.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Files `router` for `site`, replacing what was filed.
    pub fn register(&self, site: &str, router: Router) {
        self.lock().insert(site.to_owned(), router);
    }

    /// Removes what is filed for `site`.
    pub fn remove(&self, site: &str) {
        self.lock().remove(site);
    }

    /// What is filed for `site`, or an empty router.
    pub fn router_for(&self, site: &str) -> Router {
        self.lock().get(site).cloned().unwrap_or_default()
    }

    /// Whether anything is filed for `site`.
    #[must_use]
    pub fn has(&self, site: &str) -> bool {
        self.lock().contains_key(site)
    }
}

/// The metrics route with the development routes of `site` merged in (none on a host that filed
/// none, which is every non-dev host).
pub fn router_for(site: &str) -> Router {
    router().merge(dev_routes().router_for(site))
}

/// Gauge: 1 for the readiness state the process is in and 0 for the others, labelled `state`
/// (`starting`, `ready`, `degraded`, `failed`).
pub const READINESS_STATE: &str = "spot_readiness_state";

/// What a process's services say about whether it can serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadinessState {
    /// A required service has not finished starting.
    Starting,
    /// Every required service runs and no optional one is off.
    Ready,
    /// Every required service runs; an optional one (mail, print) is off or starting.
    Degraded,
    /// A required service is stopped: the site cannot serve.
    Failed,
}

impl ReadinessState {
    /// Every state, in publishing order.
    pub const ALL: [ReadinessState; 4] = [
        ReadinessState::Starting,
        ReadinessState::Ready,
        ReadinessState::Degraded,
        ReadinessState::Failed,
    ];

    /// The `state` label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReadinessState::Starting => "starting",
            ReadinessState::Ready => "ready",
            ReadinessState::Degraded => "degraded",
            ReadinessState::Failed => "failed",
        }
    }
}

/// The readiness state of `health`: the same rule the site's readiness check applies, as one
/// word. A stopped required service is `Failed` even when another is still starting.
#[must_use]
pub fn readiness_of(health: &super::Health) -> ReadinessState {
    use super::{Level, State};
    let mut starting = false;
    let mut degraded = false;
    for (level, state) in health.states().into_values() {
        match (level, state) {
            (_, State::Running) => {}
            (Level::Required, State::Stopped(_)) => return ReadinessState::Failed,
            (Level::Required, State::Starting) => starting = true,
            (Level::Optional, _) => degraded = true,
        }
    }
    if starting {
        ReadinessState::Starting
    } else if degraded {
        ReadinessState::Degraded
    } else {
        ReadinessState::Ready
    }
}

/// Publishes `state`: its series reads 1, the other three read 0.
pub fn publish_readiness(state: ReadinessState) {
    for each in ReadinessState::ALL {
        let value = if each == state { 1.0 } else { 0.0 };
        metrics::gauge!(READINESS_STATE, "state" => each.as_str()).set(value);
    }
}

/// The Prometheus side of the queue and realtime metrics: each sink call becomes one series
/// update. The crates that own the numbers define the names and the shape; this is the only place
/// that touches the recorder.
#[derive(Clone, Copy, Debug, Default)]
pub struct PrometheusSink;

impl spot_jobs::metrics::JobMetricsSink for PrometheusSink {
    fn queue_depth(&self, queue: &'static str, depth: u64) {
        metrics::gauge!(spot_jobs::metrics::JOB_QUEUE_DEPTH, "queue" => queue).set(depth as f64);
    }

    fn oldest_age_seconds(&self, queue: &'static str, seconds: u64) {
        metrics::gauge!(spot_jobs::metrics::JOB_OLDEST_AGE_SECONDS, "queue" => queue)
            .set(seconds as f64);
    }

    fn requeued(&self, queue: &'static str) {
        metrics::counter!(spot_jobs::metrics::JOB_REQUEUED_TOTAL, "queue" => queue).increment(1);
    }
}

/// The one sink every job store this host builds counts on (`PgJobStore::with_metrics`,
/// `InMemoryJobStore::with_metrics`). The sink has no state and the series carry no per-site
/// label, so the stores of all sites share it.
pub fn job_metrics_sink() -> Arc<dyn spot_jobs::metrics::JobMetricsSink> {
    static SHARED: OnceLock<Arc<dyn spot_jobs::metrics::JobMetricsSink>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| Arc::new(PrometheusSink)))
}

impl spot_mail_queue::metrics::MailMetricsSink for PrometheusSink {
    fn queue_depth(&self, depth: u64) {
        metrics::gauge!(spot_mail_queue::metrics::MAIL_QUEUE_DEPTH).set(depth as f64);
    }
}

impl spot_realtime::metrics::RealtimeMetricsSink for PrometheusSink {
    fn dropped(&self, reason: &'static str, count: u64) {
        metrics::counter!(spot_realtime::metrics::REALTIME_DROPPED_TOTAL, "reason" => reason)
            .increment(count);
    }
}

/// Registers the descriptions of the metrics this module and the process-wide ones carry, so the
/// exposition has `# HELP` lines and the metrics lint (rule M4) finds a description for each. A
/// no-op until a recorder is installed; safe to call more than once.
pub fn describe_all() {
    metrics::describe_gauge!(
        READINESS_STATE,
        "1 for the readiness state of the process (starting, ready, degraded, failed), else 0"
    );
    metrics::describe_gauge!(
        spot_jobs::metrics::JOB_QUEUE_DEPTH,
        "Jobs waiting to run, by queue (default, short, long, other)"
    );
    metrics::describe_gauge!(
        spot_jobs::metrics::JOB_OLDEST_AGE_SECONDS,
        "Seconds the oldest waiting job of the queue has waited"
    );
    metrics::describe_counter!(
        "spot_schedule_fired_total",
        "Schedule firings planned by the scheduler, by app and schedule name"
    );
    metrics::describe_gauge!(
        "spot_schedule_last_fire_seconds",
        "The instant (seconds since the epoch) the schedule last fired, by app and schedule name"
    );
    metrics::describe_counter!(
        spot_jobs::metrics::JOB_REQUEUED_TOTAL,
        "Jobs moved from dead or retrying back to the queue by a requeue, by queue"
    );
    metrics::describe_gauge!(
        spot_mail_queue::metrics::MAIL_QUEUE_DEPTH,
        "Mail messages waiting to be sent"
    );
    metrics::describe_counter!(
        spot_realtime::metrics::REALTIME_DROPPED_TOTAL,
        "Realtime events or subscriptions dropped, by reason"
    );
    // The BPM runtime (services/process.rs) records these; its constants carry no description.
    metrics::describe_counter!("spot_bpm_runs_started_total", "Process runs started");
    metrics::describe_counter!(
        "spot_bpm_runs_completed_total",
        "Process runs that completed"
    );
    metrics::describe_counter!("spot_bpm_runs_failed_total", "Process runs that failed");
    metrics::describe_counter!("spot_bpm_steps_run_total", "Process steps run");
    metrics::describe_gauge!(
        "spot_bpm_resume_backlog",
        "Process runs waiting to be resumed after a restart"
    );
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
            describe_all();
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

/// This module's start order: the one it had in the host's table before it moved here.
pub const ORDER: u32 = 900;

/// Metrics: the `/metrics` route (when enabled) and the optional recorder service, so a
/// recorder that cannot install degrades the site, not stops it.
pub async fn register(ctx: &mut SiteCtx<'_>) -> crate::Result<()> {
    let metrics = ctx.env()?.loaded.common.services.metrics.clone();
    if metrics.enabled {
        ctx.router_layer(|site_router| site_router.merge(router()));
    }
    ctx.add(Telemetry::new(metrics.enabled, metrics.require_auth));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod dev_route_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    async fn status(app: Router, path: &str) -> StatusCode {
        app.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_site_with_nothing_filed_has_no_dev_routes() {
        let routes = DevRoutes::default();
        assert!(!routes.has("acme"));
        assert_eq!(
            status(routes.router_for("acme"), "/dev/ping").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_filed_router_is_served_only_for_its_site() {
        let routes = DevRoutes::default();
        routes.register(
            "acme",
            Router::new().route("/dev/ping", get(|| async { "pong" })),
        );
        assert_eq!(
            status(routes.router_for("acme"), "/dev/ping").await,
            StatusCode::OK
        );
        assert_eq!(
            status(routes.router_for("other"), "/dev/ping").await,
            StatusCode::NOT_FOUND
        );
        routes.remove("acme");
        assert_eq!(
            status(routes.router_for("acme"), "/dev/ping").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn the_metrics_route_is_unchanged_by_the_hook() {
        // Nothing is filed for this site name in the process-wide hook.
        let app = router_for("no-such-dev-site");
        assert_ne!(status(app, "/metrics").await, StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod metric_tests {
    use super::*;
    use crate::services::{Health, Level, State};
    use spot_jobs::metrics::QueueSnapshot;
    use spot_realtime::metrics::DropReason;

    #[test]
    fn readiness_follows_the_services() {
        let health = Health::new();
        assert_eq!(
            readiness_of(&health),
            ReadinessState::Ready,
            "no services, nothing wrong"
        );
        health.set("storage", Level::Required, State::Starting);
        assert_eq!(readiness_of(&health), ReadinessState::Starting);
        health.set("storage", Level::Required, State::Running);
        health.set("mail", Level::Optional, State::Stopped("smtp down".into()));
        assert_eq!(readiness_of(&health), ReadinessState::Degraded);
        health.set("jobs", Level::Required, State::Stopped("lost".into()));
        assert_eq!(readiness_of(&health), ReadinessState::Failed);
    }

    #[test]
    fn the_readiness_labels_are_a_closed_set() {
        let names: Vec<&str> = ReadinessState::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, ["starting", "ready", "degraded", "failed"]);
    }

    #[test]
    fn every_new_metric_appears_in_the_exposition() {
        spot_telemetry::metrics::install().unwrap();
        describe_all();
        publish_readiness(ReadinessState::Degraded);
        spot_jobs::metrics::publish(
            &PrometheusSink,
            &[QueueSnapshot {
                queue: "short".to_owned(),
                depth: 3,
                oldest_age_seconds: 12,
            }],
        );
        spot_mail_queue::metrics::publish(&PrometheusSink, &[4]);
        spot_realtime::metrics::record_dropped(&PrometheusSink, DropReason::SlowConsumer, 2);
        spot_jobs::metrics::JobMetricsSink::requeued(&PrometheusSink, "long");
        let text = spot_telemetry::render_prometheus();
        for needle in [
            "spot_readiness_state{state=\"degraded\"}",
            "spot_job_queue_depth{queue=\"short\"}",
            "spot_job_oldest_age_seconds{queue=\"short\"}",
            "spot_mail_queue_depth",
            "spot_realtime_dropped_total{reason=\"slow_consumer\"}",
            "spot_job_requeued_total{queue=\"long\"}",
        ] {
            assert!(text.contains(needle), "missing {needle} in the exposition");
        }
        // Labels carry only the bounded sets: no id-like value reaches a series.
        assert!(!text.contains("tenant"), "no tenant label");
    }

    #[test]
    fn the_sink_traits_are_all_implemented_once() {
        // A compile-time check that one sink serves the three crates.
        fn takes_all<
            T: spot_jobs::metrics::JobMetricsSink
                + spot_mail_queue::metrics::MailMetricsSink
                + spot_realtime::metrics::RealtimeMetricsSink,
        >(
            _: &T,
        ) {
        }
        takes_all(&PrometheusSink);
    }
}
