//! Server assembly: builder, router, binding, connection loop, graceful shutdown.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::http::Request;
use axum::routing::get;
use hyper::body::{Frame, Incoming, SizeHint};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::config::ServerConfig;
use crate::config::ServerLimits;
use crate::error::ServerError;
use crate::health::{ReadinessCheck, ReadyState, live_handler, ready_handler};
use crate::middleware::{
    body_limit_middleware, catch_panic_middleware, span_middleware, telemetry_fields_middleware,
    timeout_middleware,
};
use crate::request::request_id_middleware;
use crate::site::{SiteResolution, SiteResolver, resolve_site_middleware};
use spot_telemetry::TelemetryLayer;

/// Assembles the router, binds the listed addresses and accepts until shutdown.
///
/// Every request runs through the same stack: request id, span, panic interception, whole
/// request timeout, body limit (declared `Content-Length` up front plus a capped body stream
/// for chunked bodies), site resolution, then site routes or the health endpoints.
pub struct ServerBuilder {
    config: ServerConfig,
    resolver: Arc<dyn SiteResolver>,
    site_routes: Router,
    checks: Vec<Arc<dyn ReadinessCheck>>,
}

impl std::fmt::Debug for ServerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerBuilder")
            .field("config", &self.config)
            .field("checks", &self.checks.len())
            .finish()
    }
}

impl ServerBuilder {
    /// New builder with the given configuration and site resolver.
    pub fn new(config: ServerConfig, resolver: impl SiteResolver) -> Self {
        ServerBuilder {
            config,
            resolver: Arc::new(resolver),
            site_routes: Router::new(),
            checks: Vec::new(),
        }
    }

    /// Mounts the site routes (resource and method endpoints from WP-PLT-05).
    pub fn site_routes(mut self, router: Router) -> Self {
        self.site_routes = router;
        self
    }

    /// Adds a readiness check; checks run concurrently on `/api/health/ready`.
    pub fn readiness_check(mut self, check: impl ReadinessCheck) -> Self {
        self.checks.push(Arc::new(check));
        self
    }

    /// Builds the router, ready to be served. Health endpoints are always mounted; site
    /// resolution runs only on paths that reach the site routes' fallbacks.
    pub fn build(self) -> Router {
        self.build_with_shutdown_flag().0
    }

    /// Builds the router and hands back the shutdown flag its ready handler shares.
    ///
    /// `serve` and `start` flip the flag themselves when their shutdown signal arrives;
    /// embedders (and tests) that need readiness to drain before the accept loop stops can
    /// flip it early: while the flag is set, `GET /api/health/ready` answers `503`.
    pub fn build_with_shutdown_flag(self) -> (Router, Arc<AtomicBool>) {
        let shutting_down = Arc::new(AtomicBool::new(false));
        let router = self.assemble(shutting_down.clone());
        (router, shutting_down)
    }

    /// Assembles the whole router: site routes under site resolution, health endpoints beside
    /// them, and the shared middleware stack around everything. The body limit is enforced in
    /// `body_limit_middleware` (declared length up front, streamed bodies via a capped
    /// `http_body_util::Limited` stream); axum's built-in 2 MiB default is disabled so
    /// `max_body_bytes` is the only limit.
    fn assemble(&self, shutting_down: Arc<AtomicBool>) -> Router {
        let limits = Arc::new(ServerLimits {
            max_body_bytes: self.config.max_body_bytes,
            request_timeout: self.config.request_timeout,
        });
        let resolution = Arc::new(SiteResolution {
            resolver: Arc::clone(&self.resolver),
            trusted_proxies: self.config.trusted_proxies.clone().into(),
            fallback_site: self.config.fallback_site.clone(),
        });
        let ready_state = Arc::new(ReadyState {
            checks: self.checks.iter().cloned().collect(),
            shutting_down,
        });
        // Site routes carry their 404/405 fallbacks inside site resolution, so unknown hosts
        // see `http.unknown_site`, known hosts see `api.*` codes.
        //
        // The telemetry layer sits inside site resolution (so `SiteField` exists when it builds
        // the span) and inside the fields middleware (so `RouteTemplate` exists), but outside
        // the handler: a panic or a 5xx is logged with the request id and the route template.
        // The request id and the panic catcher are outside it, and the panic catcher still
        // turns a caught panic into the `http.internal` envelope.
        let sites = self
            .site_routes
            .clone()
            .fallback(crate::site::unknown_route_fallback)
            .method_not_allowed_fallback(crate::site::method_not_allowed_fallback)
            .layer(TelemetryLayer::new())
            .layer(axum::middleware::from_fn(telemetry_fields_middleware))
            .layer(axum::middleware::from_fn_with_state(
                resolution,
                resolve_site_middleware,
            ));
        // Health endpoints do not resolve a site, but they are requests like any other: they get
        // the access log, the metrics and the request id.
        let health = Router::new()
            .route("/api/health/live", get(live_handler))
            .route("/api/health/ready", get(ready_handler))
            .with_state(ready_state)
            .layer(TelemetryLayer::new())
            .layer(axum::middleware::from_fn(telemetry_fields_middleware));
        Router::new()
            .merge(sites)
            .merge(health)
            .layer(axum::extract::DefaultBodyLimit::disable())
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&limits),
                body_limit_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                limits,
                timeout_middleware,
            ))
            .layer(axum::middleware::from_fn(catch_panic_middleware))
            .layer(axum::middleware::from_fn(span_middleware))
            .layer(axum::middleware::from_fn(request_id_middleware))
    }
    /// Starts the whole server loop in the background, returning the handle immediately.
    ///
    /// # Errors
    ///
    /// [`ServerError::EmptyBind`] / [`ServerError::BindFailed`].
    pub async fn start(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<ServerHandle, ServerError> {
        self.config.validate()?;
        let shutting_down = Arc::new(AtomicBool::new(false));
        let router = self.assemble(shutting_down.clone());
        let open_connections = Arc::new(AtomicUsize::new(0));
        let mut listeners = Vec::with_capacity(self.config.bind.len());
        let mut local_addrs = Vec::with_capacity(self.config.bind.len());
        for address in &self.config.bind {
            let listener =
                std::net::TcpListener::bind(address).map_err(|error| ServerError::BindFailed {
                    addr: *address,
                    source: error,
                })?;
            let local = listener
                .local_addr()
                .map_err(|error| ServerError::BindFailed {
                    addr: *address,
                    source: error,
                })?;
            listener
                .set_nonblocking(true)
                .map_err(|error| ServerError::BindFailed {
                    addr: *address,
                    source: error,
                })?;
            local_addrs.push(local);
            listeners.push(listener);
        }
        let (closed_tx, _) = watch::channel(false);
        let grace = self.config.shutdown_grace;
        let task = tokio::spawn(run_server(
            listeners,
            router,
            open_connections.clone(),
            shutting_down.clone(),
            closed_tx,
            grace,
            shutdown,
        ));
        Ok(ServerHandle {
            local_addrs,
            open_connections,
            join: task,
        })
    }

    /// Serves until `shutdown` completes: binds every address in the config, then waits for the
    /// whole loop to finish before returning.
    ///
    /// # Errors
    ///
    /// [`ServerError::EmptyBind`] / [`ServerError::BindFailed`], or anything the server loop hit.
    pub async fn serve(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServerError> {
        self.start(shutdown).await?.join().await
    }
}
/// One counted connection: while any clone of the lease lives, [`ServerHandle::open_connections`]
/// counts the connection.
///
/// The accept loop creates one lease per accepted connection and the connection's service puts a
/// clone into every request's extensions. An ordinary connection's last clone goes when its task
/// ends. An upgraded connection (a WebSocket) leaves the accept loop's task set when the upgrade
/// completes, but a handler that keeps its own clone (extract it with
/// `axum::Extension<ConnectionLease>`) holds the count until the upgraded session ends.
#[derive(Clone, Debug)]
pub struct ConnectionLease {
    _count: Arc<LeaseCount>,
}

/// The drop guard behind a [`ConnectionLease`]: decrements the server's counter once.
#[derive(Debug)]
struct LeaseCount(Arc<AtomicUsize>);

impl Drop for LeaseCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ConnectionLease {
    /// Counts one more connection on `counter`; the count drops again with the last clone.
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        ConnectionLease {
            _count: Arc::new(LeaseCount(Arc::clone(counter))),
        }
    }
}

/// The running server: bound addresses, the live connection count and the join hook.
#[derive(Debug)]
pub struct ServerHandle {
    local_addrs: Vec<SocketAddr>,
    open_connections: Arc<AtomicUsize>,
    join: tokio::task::JoinHandle<Result<(), ServerError>>,
}

impl ServerHandle {
    /// The bound addresses, one per config entry.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    /// Connections currently held, WebSockets included (an upgraded connection counts until
    /// its session ends; see [`ConnectionLease`]); returns to zero when the last client goes
    /// away after shutdown starts.
    pub fn open_connections(&self) -> usize {
        self.open_connections.load(Ordering::Relaxed)
    }

    /// Waits for the server loop to finish.
    ///
    /// # Errors
    ///
    /// Anything the server loop hit.
    pub async fn join(self) -> Result<(), ServerError> {
        match self.join.await {
            Ok(result) => result,
            Err(join) => Err(ServerError::ServerLoop(format!(
                "server task failed: {join}"
            ))),
        }
    }
}

/// One hyper service per connection, bridging tower to hyper with the peer address and the
/// connection's lease attached, and counting the requests in flight (a request is in flight
/// until its response body has been fully sent or dropped).
#[derive(Clone, Debug)]
struct PeerService {
    router: Router,
    peer: SocketAddr,
    lease: ConnectionLease,
    in_flight: Arc<watch::Sender<usize>>,
}

/// Holds one request in the connection's in-flight count until dropped.
#[derive(Debug)]
struct InFlightGuard(Arc<watch::Sender<usize>>);

impl InFlightGuard {
    fn new(counter: &Arc<watch::Sender<usize>>) -> Self {
        counter.send_modify(|count| *count += 1);
        InFlightGuard(Arc::clone(counter))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count = count.saturating_sub(1));
    }
}

/// A response body that keeps its request counted as in flight until the body is dropped.
struct TrackedBody {
    inner: axum::body::Body,
    _guard: InFlightGuard,
}

impl hyper::body::Body for TrackedBody {
    type Data = <axum::body::Body as hyper::body::Body>::Data;
    type Error = <axum::body::Body as hyper::body::Body>::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // `axum::body::Body` is `Unpin`, and so is the guard: no pin projection needed.
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl tower::Service<Request<Incoming>> for PeerService {
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: Request<Incoming>) -> Self::Future {
        tracing::debug!(peer = %self.peer, "connection");
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(self.peer));
        request.extensions_mut().insert(self.lease.clone());
        let guard = InFlightGuard::new(&self.in_flight);
        let future = self.router.call(request);
        Box::pin(async move {
            let response = future.await?;
            let (parts, body) = response.into_parts();
            Ok(axum::response::Response::from_parts(
                parts,
                axum::body::Body::new(TrackedBody {
                    inner: body,
                    _guard: guard,
                }),
            ))
        })
    }
}
/// The server loop: an accept loop per listener, one task per connection, and a driver that
/// flips the site's readiness flag on shutdown and drains everything within the grace.
async fn run_server(
    listeners: Vec<std::net::TcpListener>,
    router: Router,
    open_connections: Arc<AtomicUsize>,
    shutting_down: Arc<AtomicBool>,
    closed_tx: watch::Sender<bool>,
    grace: Duration,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), ServerError> {
    let mut accept_loops = Vec::with_capacity(listeners.len());
    for listener in listeners {
        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|error| ServerError::ServerLoop(format!("listener setup failed: {error}")))?;
        accept_loops.push(tokio::spawn(accept_loop(
            listener,
            router.clone(),
            Arc::clone(&open_connections),
            closed_tx.subscribe(),
            grace,
        )));
    }
    shutdown.await;
    tracing::debug!("shutting down");
    shutting_down.store(true, Ordering::Relaxed);
    let _ = closed_tx.send(true);
    for loop_task in accept_loops {
        if let Err(join) = loop_task.await {
            // A panicked accept loop is a bug but must not leave the join hanging.
            tracing::error!("accept loop ended: {join}");
        }
    }
    Ok(())
}

/// Accepts on one listener; on close, drains in-flight connections within the grace and stops.
async fn accept_loop(
    listener: tokio::net::TcpListener,
    router: Router,
    open_connections: Arc<AtomicUsize>,
    mut closed_rx: watch::Receiver<bool>,
    grace: Duration,
) {
    let mut conns = tokio::task::JoinSet::default();
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        let draining = deadline.is_some();
        if draining && conns.is_empty() {
            // Every in-flight connection finished before the grace ran out: stop the drain
            // immediately instead of idling for the full grace window.
            tracing::debug!("drain complete; all connections finished");
            break;
        }
        tokio::select! {
            _ = closed_rx.changed(), if !draining => {
                if *closed_rx.borrow() {
                    deadline = Some(tokio::time::Instant::now() + grace);
                }
            }
            accepted = listener.accept(), if !draining => {
                match accepted {
                    Ok((stream, peer)) => {
                        conns.spawn(serve_connection(
                            stream,
                            peer,
                            router.clone(),
                            ConnectionLease::new(&open_connections),
                            closed_rx.clone(),
                        ));
                    }
                    Err(error) => {
                        tracing::debug!(%error, "accept failed; backing off");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
            _ = grace_sleep(deadline.unwrap_or_else(|| tokio::time::Instant::now() + grace)), if draining => {
                // An aborted task drops its service and with it the lease; an upgraded
                // connection's lease lives on in its session until that ends.
                conns.abort_all();
                while conns.join_next().await.is_some() {}
                tracing::debug!("grace elapsed; closing in-flight connections");
                break;
            }
            _reaped = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

/// Sleeps until the grace deadline; only polled once draining started.
async fn grace_sleep(deadline: tokio::time::Instant) {
    tokio::time::sleep_until(deadline).await;
}

/// Resolves once no request is in flight (the guard is dropped before the future yields, so
/// the future stays `Send`).
async fn no_request_in_flight(counter: &mut watch::Receiver<usize>) {
    let _ = counter.wait_for(|count| *count == 0).await;
}

/// How long a connection with no request in flight may take to write its `GOAWAY` after
/// graceful shutdown started, before the socket is closed regardless of the client's answer.
const GOAWAY_FLUSH: Duration = Duration::from_millis(100);

/// Serves one HTTP connection on `stream`, honoring graceful shutdown. The connection counter
/// is held by `lease`, which goes with the task (abort-safe) unless an upgraded session keeps a
/// clone.
///
/// On shutdown the connection stops taking requests (HTTP/1.1: `Connection: close`; HTTP/2:
/// `GOAWAY`). hyper's HTTP/2 shutdown then waits for the client to answer a `PING` before the
/// final `GOAWAY`, and offers no knob to bound that wait, so a connection with no request in
/// flight is closed [`GOAWAY_FLUSH`] after the shutdown began, and one with a request in flight
/// when that request (response body included) ends; the accept loop's grace abort bounds the
/// rest.
async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    router: Router,
    lease: ConnectionLease,
    mut closed_rx: watch::Receiver<bool>,
) {
    let (in_flight_tx, mut in_flight_rx) = watch::channel(0usize);
    let service = TowerToHyperService::new(PeerService {
        router,
        peer,
        lease,
        in_flight: Arc::new(in_flight_tx),
    });
    let builder = auto::Builder::new(TokioExecutor::new());
    let mut conn = Box::pin(builder.serve_connection_with_upgrades(TokioIo::new(stream), service));
    tokio::select! {
        _ = conn.as_mut() => {}
        _ = closed_rx.changed() => {
            if *closed_rx.borrow() {
                // Drain: keep-alive stops, the in-flight request is served to completion.
                // The accept loop still aborts everything left when the grace expires.
                conn.as_mut().graceful_shutdown();
                tokio::select! {
                    _ = conn.as_mut() => {}
                    _ = no_request_in_flight(&mut in_flight_rx) => {
                        // Nothing left to serve: let the `GOAWAY` go out, then close without
                        // waiting for the client's answer to hyper's shutdown `PING`.
                        let _ = tokio::time::timeout(GOAWAY_FLUSH, conn.as_mut()).await;
                    }
                }
            }
        }
    }
}
