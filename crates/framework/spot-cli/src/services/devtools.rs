//! Debugging and tracing for `spot serve` in development (WP-DX-03.T3): the libraries of
//! `spot-devtools` (WP-DX-03.T1, T2) hosted under their own safety rules.
//!
//! - **Traces.** One [`TraceCollector`](spot_devtools::TraceCollector) per site, with the site's
//!   redactor and a ring of `trace_capacity` traces (default 500, 10 to 10,000), sampled at 100
//!   per cent. The [`CollectorLayer`] turns the spans the host and the engine already emit (HTTP,
//!   lifecycle, pipeline step, storage, job) into one trace per request, keyed by the request id.
//! - **The viewer.** `GET /dev/traces` and `GET /dev/traces/{id}` (JSON, or `?format=text`) on a
//!   loopback address of their own, through [`DevApi::handle_checked`](spot_devtools::DevApi): a
//!   production site answers `404`, a missing or wrong token `401`. It is **not** part of the
//!   contract router.
//! - **The debug adapter.** A Debug Adapter Protocol listener for TypeScript scripts, loopback
//!   unless `allow_remote = true`, started only when the site's script engine provides a
//!   [`DebugHost`](spot_devtools::DebugHost) port. Every launch needs the development token.
//! - **`spot trace <request-id>`** (`commands/trace.rs`) reads a trace over the viewer. The address
//!   comes from `sites/<site>/devtools.json`, written `0600` at start and removed at stop.
//!
//! # Off unless asked
//!
//! The module registers nothing unless `[services.devtools] enabled = true`. Enabled, it refuses
//! to start for a production-marked site, a release build, a non-loopback `server.bind` or trace
//! address, and a missing or short development token (`SPOT_DEV_TOKEN`, at least 16 characters;
//! never an argument, never written to disk or to a log). See [`guard::resolve`].
//!
//! # The trace layer and the route hook (WP-DX-03.T4)
//!
//! `spot-telemetry` installs one process-wide subscriber; since T4 it takes a second layer at
//! start-up (`init_with`). [`DevLayers`] is that layer: a forwarder the host attaches **once** and
//! only for `spot serve --dev` ([`dev_layers`]`().boxed_layer()`), and that this module binds its
//! per-site [`CollectorLayer`] to when the site registers. Without the attach (a non-dev start) the
//! collector stays empty and the viewer answers with an empty list. Only one site per process feeds
//! the layer (a span's extension slot holds one collector); a second site's viewer stays empty and
//! the log says so.
//!
//! [`trace_routes`] is the viewer as an axum router, handed to the host through the dev route hook
//! of `services::telemetry` ([`DevRoutes`](crate::services::telemetry::DevRoutes)); the module
//! files it there only when it registered, which it does only under the guards above.
//!
//! # What is not wired yet
//!
//! The host must call `spot_telemetry::init_with(config, Some(dev_layers().boxed_layer()))` for
//! `--dev` (it calls `init` today) and merge `telemetry::dev_routes().router_for(site)` into the
//! site router; both are outside this module's files and are listed in the task report.

#[path = "devtools/guard.rs"]
pub mod guard;
#[path = "devtools/layer.rs"]
pub mod layer;
#[path = "devtools/server.rs"]
pub mod server;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use spot_core::SiteId;
use spot_devtools::{
    DebugHost, DebugPolicy, DebugServer, DebugServerConfig, DevApi, TraceCollector,
};
use spot_redact::{RedactionPolicy, Redactor};
use tokio::sync::watch;
use tracing::Subscriber;
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

pub use guard::{Resolved, Settings, TOKEN_ENV, resolve};
pub use layer::{CollectorLayer, layer_of};

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the dev server.
pub const ORDER: u32 = 1070;

/// The file the running host writes so `spot trace` finds the viewer: `sites/<site>/devtools.json`.
#[must_use]
pub fn status_file(root: &std::path::Path, site: &str) -> PathBuf {
    root.join("sites").join(site).join("devtools.json")
}

/// The site's trace collector, with its redactor and ring.
#[must_use]
pub fn collector(site: SiteId, capacity: usize) -> TraceCollector {
    let redactor = Redactor::from_policy(&RedactionPolicy::default())
        .ok()
        .map(Arc::new);
    TraceCollector::with_redactor(site, capacity, redactor)
        .with_sampling(spot_devtools::SAMPLE_ALL_PPM)
}

/// Registers the tools when `[services.devtools] enabled = true`.
///
/// # Errors
///
/// A start that is not safe for development, or a bad setting.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("devtools")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(|e| Error(format!("devtools: {e}")))?;
    let token = std::env::var(TOKEN_ENV).ok();
    let binds = env.loaded.server.bind.clone();
    let resolved = resolve(&settings, &site, &binds, token.as_deref()).map_err(Error)?;
    let collector = collector(site.clone(), resolved.capacity);
    let owns_layer = dev_layers().bind(CollectorLayer::new(
        collector.clone(),
        Arc::clone(ctx.clock()),
    ));
    if owns_layer {
        if !dev_layers().is_attached() {
            tracing::warn!(
                site = site.as_str(),
                "the dev trace layer is not attached to the process subscriber; start with `spot serve --dev`"
            );
        }
    } else {
        tracing::warn!(
            site = site.as_str(),
            "another site already feeds the dev trace layer; this site's trace viewer stays empty"
        );
    }
    crate::services::telemetry::dev_routes().register(
        &env.site.site,
        trace_routes(
            DevApi::new(collector.clone()),
            resolved.policy.clone(),
            settings.production,
        ),
    );
    let debug = ctx.port::<dyn DebugHost>().map(|host| {
        Arc::new(DebugServer::new(
            host,
            DebugServerConfig::new(resolved.policy.clone())
                .with_address(resolved.debug_addr)
                .with_program_root(env.root.to_path_buf()),
        ))
    });
    ctx.add(Arc::new(DevTools {
        api: DevApi::new(collector),
        resolved,
        production: settings.production,
        debug,
        status_file: status_file(env.root, &env.site.site),
        site: env.site.site.clone(),
        owns_layer,
        stopping: Arc::new(AtomicBool::new(false)),
        bound: Mutex::new(None),
    }));
    Ok(())
}

/// The forwarding layer of the development tools: attached to the process subscriber once, at
/// start-up, it hands span events to the [`CollectorLayer`] a site binds later.
///
/// Cheap when nothing is bound (one read lock). A span opened before a bind is simply not traced;
/// one that closes after an unbind is closed without a trace.
#[derive(Clone, Default)]
pub struct DevLayers {
    bound: Arc<RwLock<Option<Arc<CollectorLayer>>>>,
    attached: Arc<AtomicBool>,
}

/// The process's one forwarder.
#[must_use]
pub fn dev_layers() -> &'static DevLayers {
    static LAYERS: OnceLock<DevLayers> = OnceLock::new();
    LAYERS.get_or_init(DevLayers::default)
}

impl DevLayers {
    /// The layer to pass to `spot_telemetry::init_with` (a clone sharing this forwarder's state).
    /// Marks the forwarder attached.
    #[must_use]
    pub fn boxed_layer(&self) -> spot_telemetry::BoxLayer {
        self.attached.store(true, Ordering::SeqCst);
        Box::new(self.clone())
    }

    /// Whether [`boxed_layer`](Self::boxed_layer) was called, that is whether the host attached the layer.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }

    /// Binds `layer`; `false` when another layer is bound already (the first site keeps it).
    pub fn bind(&self, layer: CollectorLayer) -> bool {
        let mut bound = self.bound.write().unwrap_or_else(|p| p.into_inner());
        if bound.is_some() {
            return false;
        }
        *bound = Some(Arc::new(layer));
        true
    }

    /// Drops the bound layer.
    pub fn unbind(&self) {
        *self.bound.write().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn current(&self) -> Option<Arc<CollectorLayer>> {
        self.bound.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl<S> tracing_subscriber::Layer<S> for DevLayers
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Some(layer) = self.current() {
            layer.on_new_span(attrs, id, ctx);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(layer) = self.current() {
            layer.on_close(id, ctx);
        }
    }
}

#[derive(Clone)]
struct TraceState {
    api: DevApi,
    policy: DebugPolicy,
    production: bool,
}

async fn trace_route(
    State(state): State<TraceState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let token = headers
        .get(server::TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let answer = state.api.handle_checked(
        &state.policy,
        state.production,
        token,
        method.as_str(),
        uri.path(),
        uri.query(),
    );
    let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        [
            (header::CONTENT_TYPE, answer.content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        answer.body,
    )
        .into_response()
}

/// The trace viewer (`GET /dev/traces`, `GET /dev/traces/{id}`) as an axum router, with the same
/// checks as the loopback listener: a production site answers `404` and a missing or wrong
/// development token (`x-dev-token`) `401`.
pub fn trace_routes(api: DevApi, policy: DebugPolicy, production: bool) -> Router {
    Router::new()
        .route("/dev/traces", get(trace_route))
        .route("/dev/traces/{id}", get(trace_route))
        .with_state(TraceState {
            api,
            policy,
            production,
        })
}

/// The running tools: the viewer's thread, the adapter's thread, and the status file.
struct DevTools {
    api: DevApi,
    resolved: Resolved,
    production: bool,
    debug: Option<Arc<DebugServer>>,
    status_file: PathBuf,
    site: String,
    /// Whether this site's collector is the one the dev trace layer feeds.
    owns_layer: bool,
    stopping: Arc<AtomicBool>,
    /// The addresses actually bound, for the status file.
    bound: Mutex<Option<(std::net::SocketAddr, Option<std::net::SocketAddr>)>>,
}

impl DevTools {
    fn write_status(&self, trace: std::net::SocketAddr, debug: Option<std::net::SocketAddr>) {
        use std::io::Write as _;
        let body = serde_json::json!({
            "trace_address": trace.to_string(),
            "debug_address": debug.map(|a| a.to_string()),
        })
        .to_string();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(&self.status_file) {
            let _ = file.write_all(body.as_bytes());
        }
    }
}

impl Service for DevTools {
    fn name(&self) -> &'static str {
        "devtools"
    }

    fn level(&self) -> Level {
        // Development tooling must never take a site out of rotation.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async {
            // The refusals were made when the module registered; a bind failure shows in `run`.
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let viewer = TcpListener::bind(self.resolved.trace_addr)
                .map_err(|e| format!("cannot bind the trace viewer: {e}"))?;
            let trace_addr = viewer.local_addr().map_err(|e| e.to_string())?;
            let adapter = match &self.debug {
                Some(_) => Some(server::bind_debug(
                    &self.resolved.policy,
                    self.resolved.debug_addr,
                )?),
                None => None,
            };
            let debug_addr = adapter.as_ref().and_then(|l| l.local_addr().ok());
            if self.resolved.policy.allow_remote {
                tracing::warn!(
                    "the devtools debug adapter may listen on a non-loopback address (allow_remote)"
                );
            }
            *self.bound.lock().unwrap_or_else(|p| p.into_inner()) = Some((trace_addr, debug_addr));
            self.write_status(trace_addr, debug_addr);

            let api = self.api.clone();
            let policy = self.resolved.policy.clone();
            let production = self.production;
            let flag = Arc::clone(&self.stopping);
            let viewer_task = tokio::task::spawn_blocking(move || {
                server::serve_traces(&viewer, &api, &policy, production, &flag);
            });
            let adapter_task = match (adapter, self.debug.clone()) {
                (Some(listener), Some(server_arc)) => {
                    let flag = Arc::clone(&self.stopping);
                    Some(tokio::task::spawn_blocking(move || {
                        server::serve_debug(&listener, &server_arc, &flag);
                    }))
                }
                _ => None,
            };
            let mut stop = stop;
            let _ = stop.wait_for(|stopped| *stopped).await;
            self.stopping.store(true, Ordering::Relaxed);
            let _ = viewer_task.await;
            if let Some(task) = adapter_task {
                let _ = task.await;
            }
            Ok(())
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        self.stopping.store(true, Ordering::Relaxed);
        if self.owns_layer {
            dev_layers().unbind();
        }
        crate::services::telemetry::dev_routes().remove(&self.site);
        let _ = std::fs::remove_file(&self.status_file);
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod layer_tests {
    use super::*;
    use spot_core::{ManualClock, SiteId};
    use tracing_subscriber::layer::SubscriberExt;

    fn bound(layers: &DevLayers) -> TraceCollector {
        let collector = TraceCollector::new(SiteId::new("acme").unwrap(), 10);
        let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
        assert!(layers.bind(CollectorLayer::new(collector.clone(), clock)));
        collector
    }

    fn one_request() {
        let http = tracing::info_span!("http_request", request_id = "r-1");
        let _http = http.enter();
        let op = tracing::info_span!("doc.op", operation = "create");
        let _op = op.enter();
    }

    #[test]
    fn a_bound_layer_collects_through_the_forwarder() {
        let layers = DevLayers::default();
        let collector = bound(&layers);
        let subscriber = tracing_subscriber::registry().with(layers.clone());
        tracing::subscriber::with_default(subscriber, one_request);
        let trace = collector.by_request("r-1").expect("a trace");
        assert_eq!(trace.spans.len(), 2);
    }

    #[test]
    fn nothing_is_collected_until_a_layer_is_bound_or_after_it_is_dropped() {
        let layers = DevLayers::default();
        let subscriber = tracing_subscriber::registry().with(layers.clone());
        tracing::subscriber::with_default(subscriber, one_request);
        let collector = bound(&layers);
        assert!(collector.by_request("r-1").is_none());
        layers.unbind();
        let subscriber = tracing_subscriber::registry().with(layers.clone());
        tracing::subscriber::with_default(subscriber, one_request);
        assert!(collector.by_request("r-1").is_none());
    }

    #[test]
    fn the_first_site_keeps_the_layer() {
        let layers = DevLayers::default();
        let _first = bound(&layers);
        let other = TraceCollector::new(SiteId::new("other").unwrap(), 10);
        let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
        assert!(!layers.bind(CollectorLayer::new(other, clock)));
    }

    #[test]
    fn attaching_is_recorded_only_when_the_host_takes_the_layer() {
        let layers = DevLayers::default();
        assert!(!layers.is_attached());
        let _boxed = layers.boxed_layer();
        assert!(layers.is_attached());
    }

    #[tokio::test]
    async fn the_routes_answer_401_without_the_token_and_404_for_production() {
        use axum::body::Body;
        use axum::http::Request;
        use spot_devtools::{DevToken, SAMPLE_ALL_PPM};
        use tower::ServiceExt as _;

        const TOKEN: &str = "dev-token-0123456789";
        let collector =
            TraceCollector::new(SiteId::new("acme").unwrap(), 10).with_sampling(SAMPLE_ALL_PPM);
        let policy = DebugPolicy::new(DevToken::new(TOKEN).unwrap());
        let app = trace_routes(DevApi::new(collector.clone()), policy.clone(), false);
        let get_with = |token: Option<&str>| {
            let mut request = Request::builder().uri("/dev/traces");
            if let Some(token) = token {
                request = request.header(server::TOKEN_HEADER, token);
            }
            request.body(Body::empty()).unwrap()
        };
        let denied = app.clone().oneshot(get_with(None)).await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let wrong = app
            .clone()
            .oneshot(get_with(Some("not-the-token-xxxx")))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let listed = app.oneshot(get_with(Some(TOKEN))).await.unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let production = trace_routes(DevApi::new(collector), policy, true);
        let hidden = production.oneshot(get_with(Some(TOKEN))).await.unwrap();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    }
}
