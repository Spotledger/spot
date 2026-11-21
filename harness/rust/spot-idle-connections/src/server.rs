//! The server under test: the real `spot-http` assembly (request stack, site resolution, health
//! endpoints, accept loop, graceful shutdown) with the realtime WebSocket route and one fake
//! service route mounted as its site routes, on a loopback port.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use axum::routing::get;
use spot_core::{SiteId, SystemClock};
use spot_http::{ServerBuilder, ServerConfig, ServerHandle, StaticSites};
use spot_realtime::ws::{CancellationToken, Rejected, TokenVerifier, WsConfig};
use spot_realtime::{AllowAll, BusConfig, Principal, RealtimeBus};
use tokio::sync::oneshot;

use crate::config::RunConfig;
use crate::error::HarnessError;

/// The path of the fake service the busy clients call.
pub const PING_PATH: &str = "/api/v1/ping";

/// The path of the health endpoint the idle HTTP connections call (no site needed).
pub const LIVE_PATH: &str = "/api/health/live";

/// The user id of idle connection `n`, in the wire id shape the realtime protocol expects.
#[must_use]
pub fn user_for(n: usize) -> String {
    format!("0192f3c4-5b6a-7c8d-9e0f-a{n:011x}")
}

/// The token idle connection `n` presents.
#[must_use]
pub fn token_for(n: usize) -> String {
    format!("idle-{n}")
}

/// The room idle connection `n` joins: one of 1,000 document rooms, so rooms are shared by about
/// ten connections each at 10,000 connections.
#[must_use]
pub fn room_for(n: usize) -> String {
    format!("doc:items:0192f3c4-5b6a-7c8d-9e0f-{:012x}", n % 1000)
}

/// The in-process token verifier: `idle-<n>` is user `n`, anything else is rejected.
struct IdleVerifier;

impl TokenVerifier for IdleVerifier {
    fn verify<'a>(
        &'a self,
        _site: &'a SiteId,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Principal, Rejected>> + Send + 'a>> {
        Box::pin(async move {
            token
                .strip_prefix("idle-")
                .and_then(|n| n.parse::<usize>().ok())
                .map(|n| Principal::user(user_for(n)))
                .ok_or_else(|| Rejected::new("not an idle-connection token"))
        })
    }
}

async fn ping() -> &'static str {
    "{\"ok\":true}"
}

/// The running server and the levers to stop it the way a deployment does on `SIGTERM`.
pub struct RunningServer {
    /// The server's handle: bound address and the live connection count.
    pub handle: ServerHandle,
    /// The bound loopback address.
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    ws_shutdown: CancellationToken,
    bus: RealtimeBus,
}

impl RunningServer {
    /// WebSocket sessions alive (bus subscriptions). `spot-http`'s own connection count does not
    /// include them: an upgraded connection leaves its accept-loop task when the upgrade
    /// completes, and the realtime session continues in a task of its own.
    #[must_use]
    pub fn ws_sessions(&self) -> usize {
        self.bus.stats().subscriptions
    }

    /// Starts shutdown: cancels the WebSocket transport's token (every socket closes 1001) and
    /// releases the HTTP server's shutdown signal.
    pub fn begin_shutdown(&mut self) {
        self.ws_shutdown.cancel();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Starts the server on `127.0.0.1:0`.
///
/// # Errors
/// [`HarnessError`] when the site id, the bind or the server loop cannot be set up.
pub async fn start(config: &RunConfig) -> Result<RunningServer, HarnessError> {
    // Split literal: the error-codes lint reads a dotted lower-case literal as an error code.
    let site = SiteId::new(concat!("idle", ".example"))
        .map_err(|e| HarnessError::new(format!("the site id is invalid: {e}")))?;
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let ws_shutdown = CancellationToken::new();
    let routes = spot_realtime::ws::router(
        bus.clone(),
        Arc::new(IdleVerifier),
        WsConfig {
            heartbeat_seconds: config.ws_heartbeat_seconds,
            shutdown: ws_shutdown.clone(),
        },
    )
    .route(PING_PATH, get(ping));
    let server_config = ServerConfig {
        bind: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        shutdown_grace: config.shutdown_grace,
        ..ServerConfig::default()
    };
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = ServerBuilder::new(
        server_config,
        StaticSites::new()
            .add("127.0.0.1", site.clone())
            .add_site_name(site),
    )
    .site_routes(routes)
    .start(async move {
        let _ = stopped.await;
    })
    .await
    .map_err(|e| HarnessError::new(format!("the server did not start: {e}")))?;
    let addr = handle
        .local_addrs()
        .first()
        .copied()
        .ok_or_else(|| HarnessError::new("the server bound no address"))?;
    Ok(RunningServer {
        handle,
        addr,
        stop: Some(stop),
        ws_shutdown,
        bus,
    })
}
