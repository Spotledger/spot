//! The native WebSocket transport (`contracts/native-api/realtime.md`, ADR-055): one route,
//! `GET /api/v1/realtime`, speaking the `spot.v1` subprotocol over an axum router that site
//! routes mount.
//!
//! # Mounting
//!
//! [`router`] returns the single-route `axum::Router` the server mounts as its site routes
//! (the same middleware stack, so the request's [`SiteContext`](spot_http::SiteContext) is
//! resolved from the host name exactly as for HTTP):
//!
//! ```ignore
//! let bus = RealtimeBus::new(BusConfig::default(), clock, authorizer);
//! let spot_routes = ws::router(bus, auth_verifier, WsConfig::default());
//! ServerBuilder::new(config, sites).site_routes(spot_routes);
//! ```
//!
//! # The handshake
//!
//! The upgrade is refused — an `api.invalid_request` envelope, not a WebSocket — unless the
//! request offers the [`SUBPROTOCOL`] (`spot.v1`). The upgrade also pins the transport's
//! frame limits to [`MAX_FRAME_BYTES`], so an oversized frame is rejected by the socket
//! layer before it can be buffered whole, and surfaces as the contract's 1009 close. The
//! first frame must then be an `auth` within [`AUTH_TIMEOUT`], checked through
//! [`TokenVerifier`]; on success the server sends `ready` and auto-joins the client to its
//! `user:<id>` room. Every close code in the contract's table is produced by the session
//! ([`session`]): 4001 for a failed or timed-out auth, 1008 for protocol violations
//! (invalid JSON, unknown types, a message before or a second `auth`), 1003 for binary
//! frames, 1009 for frames over 64 KiB, 4008 when the bus (or the stalled socket)
//! disconnects a slow consumer, 4009 when the verifier reports the session gone, 1001 on
//! server shutdown, and 1000 for a post-auth idle close. After the auth, every inbound
//! frame — the pong answers to the heartbeat included — counts as liveness; the idleness
//! close lands at exactly the 2 × `heartbeat_seconds` mark (checked as a deadline, not off
//! the heartbeat tick).
//!
//! # Events
//!
//! Delivered events are the bus's own: per-room `seq` unchanged, `payload` shared verbatim.
//! The wire adds two schema guards — an event whose `kind` is outside the contract's
//! vocabulary, whose payload is not a JSON object, or whose room name is not a wire room is
//! not emittable under the contract and is dropped with a log (a gap in that room's `seq`
//! stream then tells the client to resync; ADR-25 accepts a missing notification).

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use spot_http::SiteContext;
use spot_http::error::{ErrorDetail, ErrorEnvelope};
use spot_http::request::RequestId;
use spot_http::server::ConnectionLease;

use crate::bus::RealtimeBus;

pub mod protocol;
pub mod session;
pub mod verifier;

use session::Shared;

pub use protocol::{ClientMessage, MAX_FRAME_BYTES, MAX_ROOMS, SUBPROTOCOL, ServerMessage};
pub use tokio_util::sync::CancellationToken;
pub use verifier::{ExpiryWatch, Rejected, TokenVerifier};

/// How long the connection may wait for the first `auth` message (the contract fixes this).
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// The router's settings.
///
/// The protocol's own limits ([`MAX_FRAME_BYTES`], [`MAX_ROOMS`], `auth` first within 10 s)
/// are contract-fixed and not configurable; the heartbeat cadence and the shutdown signal
/// are deployment concerns.
#[derive(Clone, Debug)]
pub struct WsConfig {
    /// Seconds between server WebSocket pings (`ready.heartbeat_seconds`); a connection that
    /// sends nothing for twice this is closed for idleness. Contract default 25.
    pub heartbeat_seconds: u64,
    /// Cancelled when the server is draining: connections close with 1001 (**Server shutting
    /// down**) and reconnect with resume.
    pub shutdown: CancellationToken,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            heartbeat_seconds: 25,
            shutdown: CancellationToken::new(),
        }
    }
}

/// The realtime WebSocket route: `GET /api/v1/realtime`, the `spot.v1` subprotocol, the site
/// resolved by the server's site middleware. Mount as the site routes:
/// `ServerBuilder::…site_routes(router(bus, verifier, config))`.
///
/// The upgrade carries the contract's own frame bound: `max_message_size` and
/// `max_frame_size` are both [`MAX_FRAME_BYTES`], so the socket layer rejects an oversized
/// frame as it decodes instead of first buffering up to the 64 MiB library default — and a
/// client cannot make the server allocate past 64 KiB per message, including before `auth`.
pub fn router(
    bus: RealtimeBus,
    verifier: Arc<dyn TokenVerifier>,
    config: WsConfig,
) -> axum::Router {
    // The cadence is clamped to the contract schema's minimum of 1 s.
    let shared = Arc::new(Shared {
        bus,
        verifier,
        heartbeat_seconds: config.heartbeat_seconds.max(1),
        shutdown: config.shutdown,
    });
    axum::Router::new().route("/api/v1/realtime", get(upgrade).with_state(shared))
}

/// Upgrades `GET /api/v1/realtime` when the request speaks `spot.v1`; refuses with the
/// native API's error envelope otherwise — a request that does not upgrade to a WebSocket is
/// one the contract does not describe (`api.invalid_request`, 400), and so is one that offers
/// no `spot.v1` subprotocol. There is no close code to send for either: no socket was ever
/// accepted.
async fn upgrade(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    site: SiteContext,
    lease: Option<Extension<ConnectionLease>>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let ws = match ws {
        Ok(ws) => ws,
        Err(_) => return refuse(&site.request_id, "This endpoint is a `spot.v1` WebSocket."),
    };
    if !offered_subprotocol(&headers).is_some_and(|protocol| protocol == SUBPROTOCOL) {
        return refuse(
            &site.request_id,
            "The realtime endpoint upgrades only with the `spot.v1` subprotocol.",
        );
    }
    let site_id = site.site;
    ws.protocols([SUBPROTOCOL])
        // The contract's 64 KiB frame bound, enforced by the socket itself: tungstenite
        // refuses the frame as it decodes (no 64-MiB buffering), and the session maps the
        // capacity rejection onto the contract's 1009 close.
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_failed_upgrade(|error| tracing::debug!(%error, "realtime upgrade failed"))
        .on_upgrade(move |socket| {
            // The session keeps the server's connection lease, so `open_connections()` counts
            // the WebSocket until the session task ends.
            let lease = lease.map(|Extension(lease)| lease);
            session::run(shared, site_id, socket, lease)
        })
}

/// The `Sec-WebSocket-Protocol` token list of the request, if any.
fn offered_subprotocol(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get("sec-websocket-protocol")?.to_str().ok()?;
    raw.split(',')
        .map(str::trim)
        .find(|token| !token.is_empty())
}

/// The 400 envelope for a request the realtime route does not describe.
fn refuse(request_id: &RequestId, message: &'static str) -> Response {
    let envelope = ErrorEnvelope {
        error: ErrorDetail {
            code: "api.invalid_request",
            message: message.to_owned(),
            retryable: false,
            request_id: request_id.as_str().to_owned(),
        },
    };
    (StatusCode::BAD_REQUEST, Json(envelope)).into_response()
}
