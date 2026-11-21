//! Sync catch-up wired to the hosts (WP-RTB-02.T15, ADR-066). Operator guide:
//! `docs/running-spot/sync_catchup_wiring.md`.
//!
//! `sync_catchup` (the director) routes a returning node and `sync_catchup_serve` cuts and applies
//! chunks; neither is called by anything. This module is the call site, in two halves that share
//! one small request/answer shape ([`RouteReply`], [`WireChunk`], [`AckReply`]):
//!
//! # Hub
//!
//! [`CatchupHub`] answers three questions for the node the sync listener authenticated: *how do I
//! come back* ([`CatchupHub::route`]: the director decides from the node's gap, never from what
//! the node says), *give me chunk n* ([`CatchupHub::chunk`]: a bootstrap chunk from the verified
//! snapshot or a piece of the coalesced plan, never past the next unacknowledged chunk) and
//! *chunk n is applied* ([`CatchupHub::ack`]: the director moves only here). What the hub knows
//! about a node's gap (its applied prefix, the stream's head, the planned events) comes from the
//! hub host through the [`CatchupFeed`] port.
//!
//! The same three calls are mounted as routes ([`CatchupRoutes`], under `/sync/v1/catchup/`) that
//! read the node from the authenticated identity, so the sync listener only has to merge one
//! router behind its guard (see the operator guide for the one-line hook the hub host needs). A
//! hub whose prerequisites are missing still provides the routes: they answer every call with the
//! catalogued `sync_catchup_serve.not_ready` instead of a 404 that looks like "no such hub".
//!
//! # Node
//!
//! [`CatchupNode`] drives the [`ChunkApplier`] from a [`ChunkSource`] (the node host's connection
//! to the hub): [`CatchupNode::begin`] asks the hub how to come back, [`CatchupNode::step`] fetches
//! the next chunk, applies it and only then acknowledges it. An apply failure acknowledges
//! nothing, so the node stays at its previous checkpoint and the hub's director stays on the
//! same chunk: no half-applied chunk is ever reported as done.
//!
//! # Not ready
//!
//! On a hub the module needs the director (`sync_catchup`), the chunk server (`sync_catchup_serve`)
//! and a [`CatchupFeed`]; on an edge the applier (`sync_catchup_serve`) and a [`ChunkSource`].
//! Anything absent is named in readiness and nothing is wired on a guess. A standalone site wires
//! nothing. Settings `[services.sync_catchup_wiring]`: `enabled` (default true).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde::{Deserialize, Serialize};
use spot_core::Clock;
use spot_sync_bootstrap::{HubHead, NodeState};
use spot_sync_catchup::{CatchUpPlan, GapSummary};
use spot_sync_catchup_host::{
    Applied, ChunkAck, ChunkApplier, ChunkServer, Director, Route, ServeError, WireChunk,
};
use spot_sync_http::AuthenticatedNode;

use crate::services::node_role::{ActiveRole, Role};
use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result};

/// After `sync_catchup` (1150) and `sync_catchup_serve` (1155), whose ports it uses, and before
/// the sync hub (1300), which merges the routes.
pub const ORDER: u32 = 1160;

/// The path prefix of the three routes.
pub const PREFIX: &str = "/sync/v1/catchup";

/// `[services.sync_catchup_wiring]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` wires nothing.
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

// ---- the shape both halves speak --------------------------------------------------------------

/// How a returning node comes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum RouteReply {
    /// Close behind: stay on the live stream, no chunks.
    Live,
    /// A coalesced, dependency-ordered catch-up; ask for chunks from this index.
    CatchUp {
        /// The first chunk to ask for.
        resume_from_chunk: u64,
    },
    /// A snapshot bootstrap; ask for chunks from this index.
    Bootstrap {
        /// The first chunk to ask for.
        resume_from_chunk: u64,
    },
    /// No bootstrap slot now: ask again after this many milliseconds.
    Wait {
        /// Milliseconds to wait.
        retry_after_ms: u64,
    },
}

/// The answer to "chunk n is applied".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ack", rename_all = "snake_case")]
pub enum AckReply {
    /// Recorded; ask for the next chunk.
    Next,
    /// Recorded, but wait before the next chunk (the node is out of chunk tokens).
    Wait {
        /// Milliseconds to wait.
        retry_after_ms: u64,
    },
    /// The last chunk: the plan is complete and the stream continues.
    Complete,
}

/// Why a catch-up call was refused. Codes and counts; never a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatchupError {
    /// The serving or applying refused (its catalogued code follows).
    Serve(ServeError),
    /// The hub has no slot or token for the node now.
    Wait {
        /// Milliseconds to wait before asking again.
        retry_after_ms: u64,
    },
}

impl CatchupError {
    /// The stable catalogue code (`sync_catchup_serve.*`). A wait is reported as `not_ready`: the
    /// hub is not ready for this node yet.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Serve(error) => error.code(),
            Self::Wait { .. } => "sync_catchup_serve.not_ready",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Wait { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Serve(error) => match error {
                ServeError::NotReady(_) | ServeError::NoVerifiedSnapshot => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                ServeError::ScopeUnknown => StatusCode::FORBIDDEN,
                ServeError::OutOfRange | ServeError::OutOfOrder => StatusCode::CONFLICT,
                ServeError::Tampered | ServeError::Encode => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
        }
    }
}

impl From<ServeError> for CatchupError {
    fn from(error: ServeError) -> Self {
        Self::Serve(error)
    }
}

impl std::fmt::Display for CatchupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl std::error::Error for CatchupError {}

/// The body of a refusal: the code and, for a wait, how long. Nothing else.
#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
}

impl IntoResponse for CatchupError {
    fn into_response(self) -> Response {
        let retry_after_ms = match &self {
            Self::Wait { retry_after_ms } => Some(*retry_after_ms),
            Self::Serve(_) => None,
        };
        let body = ErrorBody {
            code: self.code().to_owned(),
            retry_after_ms,
        };
        (self.status(), Json(body)).into_response()
    }
}

// ---- the hub ----------------------------------------------------------------------------------

/// What the hub knows about one returning node, read when the node asks.
#[derive(Clone, Debug)]
pub struct NodeView {
    /// The node's identity, tenant, generation and applied prefix (from the hub's directory and
    /// the node's last acknowledged position, never from a request body).
    pub state: NodeState,
    /// The stream's head now.
    pub head: HubHead,
    /// The gap, for the director's mode decision.
    pub summary: GapSummary,
}

/// The hub host's view of a node's gap and the coalesced plan for it.
///
/// Blocking, like the chunk server: the routes call it on the blocking pool. It is the one thing
/// this module cannot build: it needs the hub's stream, the outbox and the journal segments.
pub trait CatchupFeed: Send + Sync {
    /// The node's gap, or `None` for a node the hub does not know.
    fn view(&self, node: &str) -> Option<NodeView>;

    /// The coalesced catch-up plan for the node, scoped for it, or `None` when none can be made.
    fn plan(&self, node: &str) -> Option<CatchUpPlan>;
}

/// The hub's three catch-up calls, independent of the transport.
pub struct CatchupHub {
    director: Arc<Director>,
    server: Arc<ChunkServer>,
    feed: Arc<dyn CatchupFeed>,
    clock: Arc<dyn Clock>,
}

fn now_ms(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

fn reply_of(route: &Route) -> RouteReply {
    match route {
        Route::Live => RouteReply::Live,
        Route::CatchUp {
            resume_from_chunk, ..
        } => RouteReply::CatchUp {
            resume_from_chunk: *resume_from_chunk,
        },
        Route::Bootstrap { resume_from_chunk } => RouteReply::Bootstrap {
            resume_from_chunk: *resume_from_chunk,
        },
        Route::Wait { retry_after_ms } => RouteReply::Wait {
            retry_after_ms: *retry_after_ms,
        },
    }
}

impl CatchupHub {
    /// A hub over the director and chunk server of `sync_catchup` and `sync_catchup_serve`.
    #[must_use]
    pub fn new(
        director: Arc<Director>,
        server: Arc<ChunkServer>,
        feed: Arc<dyn CatchupFeed>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            director,
            server,
            feed,
            clock,
        }
    }

    fn view(&self, node: &str) -> std::result::Result<NodeView, CatchupError> {
        self.feed
            .view(node)
            .ok_or(CatchupError::Serve(ServeError::ScopeUnknown))
    }

    fn route_for(&self, node: &str) -> std::result::Result<(Route, NodeView), CatchupError> {
        let view = self.view(node)?;
        let route = self
            .director
            .route(node, &view.summary, now_ms(&*self.clock))
            .map_err(|e| CatchupError::Serve(ServeError::from(e)))?;
        Ok((route, view))
    }

    /// How `node` comes back. A node with a stored plan resumes it, whatever its gap now says.
    ///
    /// # Errors
    /// [`CatchupError`]: an unknown node, or the director's bookkeeping.
    pub fn route(&self, node: &str) -> std::result::Result<RouteReply, CatchupError> {
        self.route_for(node).map(|(route, _)| reply_of(&route))
    }

    /// Chunk `index` for `node`. Served again as often as asked (a cut repeats at most the chunk in
    /// flight); never past the next unacknowledged chunk.
    ///
    /// # Errors
    /// [`CatchupError`]: a node that is not catching up (`out_of_range`), a chunk past the next
    /// one (`chunk_out_of_order`), a wait, or the serving's own refusal.
    pub fn chunk(&self, node: &str, index: u64) -> std::result::Result<WireChunk, CatchupError> {
        // The route is recomputed, not remembered: a hub that restarted mid-plan resumes from the
        // director's stored progress, and a node never chooses its own mode.
        let (route, view) = self.route_for(node)?;
        let next = match route {
            Route::Live => return Err(ServeError::OutOfRange.into()),
            Route::Wait { retry_after_ms } => return Err(CatchupError::Wait { retry_after_ms }),
            Route::CatchUp {
                resume_from_chunk, ..
            }
            | Route::Bootstrap { resume_from_chunk } => resume_from_chunk,
        };
        if index > next {
            return Err(ServeError::OutOfOrder.into());
        }
        match route {
            Route::Bootstrap { .. } => {
                Ok(self.server.serve_bootstrap(&view.state, view.head, index)?)
            }
            Route::CatchUp { .. } => {
                let plan = self
                    .feed
                    .plan(node)
                    .ok_or(ServeError::NotReady("a catch-up plan for the node"))?;
                Ok(self.server.serve_catch_up(node, &plan, index)?)
            }
            Route::Live | Route::Wait { .. } => Err(ServeError::OutOfRange.into()),
        }
    }

    /// `node` reports chunk `index` applied.
    ///
    /// # Errors
    /// [`CatchupError`]: an acknowledgement out of order, or the director's bookkeeping.
    pub fn ack(&self, node: &str, index: u64) -> std::result::Result<AckReply, CatchupError> {
        let ack = self.server.ack(node, index, now_ms(&*self.clock))?;
        Ok(match ack {
            ChunkAck::Next => AckReply::Next,
            ChunkAck::Wait(retry_after_ms) => AckReply::Wait { retry_after_ms },
            ChunkAck::Complete => AckReply::Complete,
        })
    }
}

// ---- the routes -------------------------------------------------------------------------------

/// The catch-up routes, ready to merge behind the sync listener's guard. A port.
#[derive(Clone)]
pub struct CatchupRoutes {
    router: Router,
}

impl CatchupRoutes {
    /// The routes of a serving hub.
    #[must_use]
    pub fn serving(hub: Arc<CatchupHub>) -> Self {
        let router = Router::new()
            .route(&format!("{PREFIX}/route"), post(route_handler))
            .route(&format!("{PREFIX}/chunk"), get(chunk_handler))
            .route(&format!("{PREFIX}/ack"), post(ack_handler))
            .with_state(hub);
        Self { router }
    }

    /// The routes of a hub that cannot serve yet: every call answers `not_ready`, naming nothing
    /// about the site.
    #[must_use]
    pub fn not_ready() -> Self {
        let router = Router::new().route(&format!("{PREFIX}/{{*rest}}"), any(not_ready_handler));
        Self { router }
    }

    /// The router to merge into the sync listener's.
    pub fn router(&self) -> Router {
        self.router.clone()
    }
}

#[derive(Deserialize)]
struct IndexQuery {
    index: u64,
}

/// Runs a blocking hub call on the blocking pool and answers JSON.
async fn blocking<T, F>(work: F) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> std::result::Result<T, CatchupError> + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => CatchupError::Serve(ServeError::NotReady("the catch-up worker")).into_response(),
    }
}

async fn route_handler(
    State(hub): State<Arc<CatchupHub>>,
    axum::Extension(node): axum::Extension<AuthenticatedNode>,
) -> Response {
    let who = node.node.as_str().to_owned();
    blocking(move || hub.route(&who)).await
}

async fn chunk_handler(
    State(hub): State<Arc<CatchupHub>>,
    axum::Extension(node): axum::Extension<AuthenticatedNode>,
    Query(query): Query<IndexQuery>,
) -> Response {
    let who = node.node.as_str().to_owned();
    blocking(move || hub.chunk(&who, query.index)).await
}

async fn ack_handler(
    State(hub): State<Arc<CatchupHub>>,
    axum::Extension(node): axum::Extension<AuthenticatedNode>,
    Query(query): Query<IndexQuery>,
) -> Response {
    let who = node.node.as_str().to_owned();
    blocking(move || hub.ack(&who, query.index)).await
}

async fn not_ready_handler() -> Response {
    CatchupError::Serve(ServeError::NotReady("the catch-up serving")).into_response()
}

// ---- the node ---------------------------------------------------------------------------------

/// The node host's connection to the hub's catch-up calls.
pub trait ChunkSource: Send + Sync {
    /// How to come back.
    ///
    /// # Errors
    /// The hub's refusal, or the connection's.
    fn route(&self) -> std::result::Result<RouteReply, CatchupError>;

    /// Chunk `index`.
    ///
    /// # Errors
    /// As [`ChunkSource::route`].
    fn chunk(&self, index: u64) -> std::result::Result<WireChunk, CatchupError>;

    /// Chunk `index` is applied.
    ///
    /// # Errors
    /// As [`ChunkSource::route`].
    fn ack(&self, index: u64) -> std::result::Result<AckReply, CatchupError>;
}

/// What one [`CatchupNode::step`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A chunk was applied and acknowledged; call `step` again.
    Applied,
    /// The hub asks the node to wait before the next chunk.
    Wait {
        /// Milliseconds to wait.
        retry_after_ms: u64,
    },
    /// The last chunk was applied: switch to the live stream (for a bootstrap, from
    /// `resume_from`).
    Done {
        /// `P + 1` for a bootstrap, `None` for a catch-up.
        resume_from: Option<u64>,
    },
}

/// The node's catch-up: the hub's chunks into the node's apply path.
pub struct CatchupNode {
    applier: Arc<ChunkApplier>,
    source: Arc<dyn ChunkSource>,
}

impl CatchupNode {
    /// A node over its applier and its connection to the hub.
    #[must_use]
    pub fn new(applier: Arc<ChunkApplier>, source: Arc<dyn ChunkSource>) -> Self {
        Self { applier, source }
    }

    /// Asks the hub how to come back. A stored checkpoint wins over the hub's resume index only
    /// in that the node asks for its own next chunk, which the hub never refuses (it is at most
    /// the next unacknowledged one).
    ///
    /// # Errors
    /// The hub's refusal or the connection's.
    pub fn begin(&self) -> std::result::Result<RouteReply, CatchupError> {
        self.source.route()
    }

    /// Fetches the node's next chunk, applies it and then acknowledges it.
    ///
    /// # Errors
    /// [`CatchupError`]: a tampered chunk, a failed apply (the checkpoint did not move and nothing
    /// was acknowledged), the hub's refusal or the connection's.
    pub fn step(&self) -> std::result::Result<Step, CatchupError> {
        let index = self.applier.next_index()?;
        let wire = self.source.chunk(index)?;
        match self.applier.apply(&wire)? {
            Applied::Duplicate => Err(ServeError::OutOfOrder.into()),
            Applied::Next => match self.source.ack(wire.index)? {
                AckReply::Next => Ok(Step::Applied),
                AckReply::Wait { retry_after_ms } => Ok(Step::Wait { retry_after_ms }),
                // The hub thinks the plan is complete before the node saw the last chunk.
                AckReply::Complete => Err(ServeError::OutOfOrder.into()),
            },
            Applied::Complete { resume_from } => {
                self.source.ack(wire.index)?;
                Ok(Step::Done { resume_from })
            }
        }
    }
}

// ---- registration -----------------------------------------------------------------------------

/// What a site wired, kept across a reload (a reload must not build a second one).
#[derive(Clone, Default)]
struct Built {
    hub: Option<Arc<CatchupHub>>,
    routes: Option<CatchupRoutes>,
    node: Option<Arc<CatchupNode>>,
}

fn built() -> &'static Mutex<HashMap<String, Built>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Built>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// The catch-up hub of a running hub site (for a harness).
#[must_use]
pub fn hosted_hub(site: &str) -> Option<Arc<CatchupHub>> {
    built()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .and_then(|b| b.hub.clone())
}

/// Says what is missing, in the log and in readiness.
fn not_ready(ctx: &mut SiteCtx<'_>, missing: &[&'static str]) {
    let reason = format!(
        "sync_catchup_serve.not_ready: {} not provided",
        missing.join(", ")
    );
    tracing::info!(code = "sync_catchup_serve.not_ready", missing = %missing.join(", "), "sync_catchup_wiring is not ready");
    ctx.add(Idle::new(
        "sync_catchup_wiring",
        Level::Optional,
        move || {
            let reason = reason.clone();
            Box::pin(async move { Err(reason) })
        },
    ));
}

fn provide_all(ctx: &mut SiteCtx<'_>, built: &Built) -> Result<()> {
    if let Some(hub) = &built.hub {
        ctx.provide::<CatchupHub>(Arc::clone(hub))?;
    }
    if let Some(routes) = &built.routes {
        ctx.provide::<CatchupRoutes>(Arc::new(routes.clone()))?;
    }
    if let Some(node) = &built.node {
        ctx.provide::<CatchupNode>(Arc::clone(node))?;
    }
    Ok(())
}

fn wire_hub(ctx: &mut SiteCtx<'_>) -> Built {
    let director = ctx.port::<Director>();
    let server = ctx.port::<ChunkServer>();
    let feed = ctx.port::<dyn CatchupFeed>();
    let mut missing = Vec::new();
    if director.is_none() {
        missing.push("the catch-up director");
    }
    if server.is_none() {
        missing.push("the chunk server");
    }
    if feed.is_none() {
        missing.push("the hub's catch-up feed");
    }
    let (Some(director), Some(server), Some(feed)) = (director, server, feed) else {
        not_ready(ctx, &missing);
        // The routes still exist and answer with the catalogued error: a node learns "not ready"
        // rather than "no such route".
        return Built {
            routes: Some(CatchupRoutes::not_ready()),
            ..Built::default()
        };
    };
    let hub = Arc::new(CatchupHub::new(
        director,
        server,
        feed,
        Arc::clone(ctx.clock()),
    ));
    Built {
        routes: Some(CatchupRoutes::serving(Arc::clone(&hub))),
        hub: Some(hub),
        node: None,
    }
}

fn wire_edge(ctx: &mut SiteCtx<'_>) -> Built {
    let applier = ctx.port::<ChunkApplier>();
    let source = ctx.port::<dyn ChunkSource>();
    let mut missing = Vec::new();
    if applier.is_none() {
        missing.push("the chunk applier");
    }
    if source.is_none() {
        missing.push("the node's connection to the hub");
    }
    let (Some(applier), Some(source)) = (applier, source) else {
        not_ready(ctx, &missing);
        return Built::default();
    };
    Built {
        node: Some(Arc::new(CatchupNode::new(applier, source))),
        ..Built::default()
    }
}

/// Wires catch-up on a hub (the call site of the chunk server) and on an edge (the call site of
/// the applier), each only when its prerequisites are provided.
///
/// # Errors
/// A port provided twice.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("sync_catchup_wiring")?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        let existing = built()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&site)
            .cloned();
        if let Some(existing) = existing {
            provide_all(ctx, &existing)?;
        }
        return Ok(());
    }
    let Some(active) = ctx.port::<ActiveRole>() else {
        return Ok(());
    };
    let wired = match active.role() {
        Role::Standalone => Built::default(),
        Role::Hub => wire_hub(ctx),
        Role::Edge => wire_edge(ctx),
    };
    provide_all(ctx, &wired).map_err(|e| Error(format!("services.sync_catchup_wiring: {e}")))?;
    built()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(site, wired);
    Ok(())
}
