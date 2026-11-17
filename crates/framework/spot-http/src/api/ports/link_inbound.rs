//! The seam of the external-peer inbound webhook, `POST /api/v1/link/inbound/{peer}` (WP-CTL-11.T4,
//! ADR-058 rule 12).
//!
//! The route is **internet-facing and unauthenticated by bearer**: the peer proves itself by an
//! HMAC signature over the raw body. Everything that decides is behind this trait; the route
//! only reads the body under a cap and renders two answers:
//!
//! - `202 {"accepted": true}`;
//! - `401 link_inbound.unauthorized`, the same status, headers and body for **every** refusal
//!   (an unknown peer, a bad or missing signature, a stale timestamp, a replay, a source address
//!   outside the allow-list, a body over the peer's cap, a malformed peer name), so the endpoint
//!   never reveals whether a peer exists;
//! - `429 link_inbound.rate_limited` when the per-peer limit is spent. The limit is charged to
//!   the **name in the path**, known or not, before the peer is looked up, so the answer does not
//!   depend on whether the peer exists either.
//!
//! This file declares no `KEY`: the group is not in the port registry because the operation is
//! not (yet) in the native API contract; the host mounts the route itself (see
//! `routes/link_inbound.rs`).

use std::net::IpAddr;

use axum::body::Bytes;
use spot_core::SiteId;

use crate::api::service::BoxFuture;

/// The body cap used for any peer name the port does not know (and the ceiling of the route's
/// own read): one mebibyte, the documented default of a peer.
pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// Why a request was not accepted. The route renders each as one fixed answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkInboundRefusal {
    /// Refused: `401`, whatever the cause.
    Rejected,
    /// The per-peer limit is spent: `429`.
    RateLimited,
}

/// One request, as far as the route knows it.
#[derive(Clone, Debug)]
pub struct InboundCall {
    /// The site the request resolved to.
    pub site: SiteId,
    /// The peer name taken from the path, as received (already checked to be a plain name).
    pub peer: String,
    /// The request path.
    pub path: String,
    /// The address the request is attributed to (the connection, or the forwarded client of a
    /// trusted proxy); `None` when unknown.
    pub source: Option<IpAddr>,
    /// The request headers that are valid text, names as received.
    pub headers: Vec<(String, String)>,
}

/// The receiver of external-peer webhooks.
pub trait LinkInboundPort: Send + Sync {
    /// The first gate, before any body is read: charges the per-peer rate limit and checks the
    /// peer's source-address allow-list. Returns the body cap to read under: the peer's own,
    /// or [`DEFAULT_MAX_BODY_BYTES`] for a name that is not a peer.
    ///
    /// # Errors
    /// [`LinkInboundRefusal`].
    fn admit(&self, call: &InboundCall) -> Result<usize, LinkInboundRefusal>;

    /// Verifies and applies one request: the signature over the raw `body`, the timestamp window,
    /// the durable replay check, then the tenant link's own receive path (grant limits and the
    /// field list). `Ok(())` is an accepted request; every refusal is
    /// [`LinkInboundRefusal::Rejected`].
    fn receive<'a>(
        &'a self,
        call: InboundCall,
        body: Bytes,
    ) -> BoxFuture<'a, Result<(), LinkInboundRefusal>>;
}
