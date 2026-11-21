//! `link_inbound` (`POST /api/v1/link/inbound/{peer}`), native API 0.26.0, WP-CTL-11.T4: the
//! inbound webhook of an external peer.
//!
//! **A public group** (`security: []` by contract): the registry mounts it outside the bearer
//! layer, behind the `auth` rate limit per client address, and only when the host provides a
//! [`LinkInboundPort`]. The external link service provides the port only when it is enabled, so
//! the route exists **only when the service is enabled**; without the port the group is skipped
//! and the path answers as any unserved operation does.
//!
//! # What the route does
//!
//! 1. the path name must be a plain peer name (`^[a-z0-9][a-z0-9_-]{0,63}$`);
//! 2. [`LinkInboundPort::admit`]: the per-peer rate limit, then the source allow-list, then the
//!    body cap to read under;
//! 3. a declared length over the cap, or a body that passes the cap while it streams, is refused
//!    (the read stops at the cap; an endless body is never buffered past it);
//! 4. [`LinkInboundPort::receive`] with the raw bytes: the route never parses the body.
//!
//! Every refusal except the rate limit is the one `401`: same status, same headers, same body, no
//! request id, nothing of the request echoed. A body over the cap is a `401` too (a `413` would
//! say the peer exists and what its cap is). The route logs the refusal class and never a value.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;

use super::{Public, RateClass, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::link_inbound::{InboundCall, LinkInboundPort, LinkInboundRefusal};
use crate::site::SiteContext;

/// The operation ids this group serves.
pub const SERVES: &[&str] = &["link_inbound"];

/// The group is public: no bearer, the `auth` rate class.
pub const PUBLIC: Public = Public {
    rate_class: RateClass::Auth,
};

/// The path template of the route.
pub const PATH: &str = "/api/v1/link/inbound/{peer}";

/// The ports this group needs: the inbound port of the external link service.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn LinkInboundPort>()];
    &KEYS
}

/// The peer name's documented form: `^[a-z0-9][a-z0-9_-]{0,63}$`.
fn is_peer_name(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes
            .iter()
            .skip(1)
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == 0x5f || *b == 0x2d)
}

fn answer(status: StatusCode, body: &'static str) -> Response {
    let mut response = Response::new(axum::body::Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The one refusal.
fn unauthorized() -> Response {
    tracing::info!(route = "link_inbound", class = "refused", "request refused");
    answer(
        StatusCode::UNAUTHORIZED,
        "{\"error\":{\"code\":\"link_inbound.unauthorized\"}}",
    )
}

fn rate_limited() -> Response {
    tracing::info!(
        route = "link_inbound",
        class = "rate_limited",
        "request refused"
    );
    let mut response = answer(
        StatusCode::TOO_MANY_REQUESTS,
        "{\"error\":{\"code\":\"api.rate_limited\"}}",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

/// The route (state: a [`RouteState`] holding the [`LinkInboundPort`]).
pub fn routes() -> Router<RouteState> {
    Router::new().route("/api/v1/link/inbound/{peer}", post(link_inbound))
}

fn headers_of(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|text| (name.as_str().to_owned(), text.to_owned()))
        })
        .collect()
}

async fn link_inbound(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(peer): Path<String>,
    request: Request,
) -> Response {
    // Not reachable through the registry (the group is skipped without its port); the refusal
    // is the uniform one so a half-wired state still says nothing about peers.
    let Some(port) = state.port::<dyn LinkInboundPort>() else {
        return unauthorized();
    };
    if !is_peer_name(&peer) {
        return unauthorized();
    }
    // Bound first: the route scanner reads `field: format!(..)` as a path-qualified macro.
    let path = format!("/api/v1/link/inbound/{peer}");
    let call = InboundCall {
        site: site.site.clone(),
        path,
        peer,
        source: site.peer,
        headers: headers_of(&headers),
    };
    let cap = match port.admit(&call) {
        Ok(cap) => cap,
        Err(LinkInboundRefusal::RateLimited) => return rate_limited(),
        Err(LinkInboundRefusal::Rejected) => return unauthorized(),
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok());
    if declared.is_some_and(|n| n > cap) {
        return unauthorized();
    }
    // The read stops at the cap, so an endless body is never buffered past it.
    let Ok(body) = axum::body::to_bytes(request.into_body(), cap).await else {
        return unauthorized();
    };
    match port.receive(call, body).await {
        Ok(()) => answer(StatusCode::ACCEPTED, "{\"accepted\":true}"),
        Err(LinkInboundRefusal::RateLimited) => rate_limited(),
        Err(LinkInboundRefusal::Rejected) => unauthorized(),
    }
}
