//! `auth_consumer_renew` (`POST /api/v1/auth/consumer/renew`), contract 0.10.0, WP-CON-03.T14.
//!
//! The renewal cookie is the only credential: no bearer is read, no body, no query (a query is
//! refused before the cookie is looked at, so a token in a URL is never accepted). The route
//! reads `__Host-spot_consumer_renew` only here. A missing, repeated, malformed, unknown,
//! expired, revoked or replayed cookie is the one `401 auth.session_expired`, with no
//! `Set-Cookie`; a success rotates the cookie (`Max-Age` 30 days, `Path=/`, `Secure`,
//! `HttpOnly`, `SameSite=Strict`) and answers the new token in the body. Every answer is
//! `Cache-Control: no-store`. No secret is logged or put in an error.
//!
//! **A public group.** The operation has no bearer by contract (`security: []`), so the file
//! declares [`PUBLIC`] (rate class `auth`): the registry mounts it outside the bearer layer,
//! behind the per-address rate limit, which runs before this handler reads the cookie. The
//! router removes any `Authorization` header before the handler runs.
//!
//! **Cross-site refusal.** Defence in depth on top of `SameSite=Strict`: a request whose
//! `Origin` is present and is not the site's own origin (an exact scheme, host and port
//! comparison with the request's effective host, [`SiteContext::host`]: the `X-Forwarded-Host` from a
//! trusted proxy, else `Host`; a request with no `SiteContext` is refused when it has an `Origin`, never a substring match), or whose
//! `Sec-Fetch-Site` is `cross-site`, is refused with the generic forbidden error `403
//! permission.denied` before the cookie is read, so a refused request never consumes, rotates
//! or revokes a cookie or its family. Requests with neither header (native apps, non-browser
//! clients) are allowed. The contract lists no `403` for this operation: a contract item.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::json;

use super::{Public, RateClass, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::consumer_sessions::{
    COOKIE_MAX_AGE_SECS, COOKIE_NAME, ConsumerSessions, RenewError,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::render_error;

/// The operation ids this group serves.
pub const SERVES: &[&str] = &["auth_consumer_renew"];

/// The group is public: no bearer, the `auth` rate class.
pub const PUBLIC: Public = Public {
    rate_class: RateClass::Auth,
};

/// The ports this group needs.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn ConsumerSessions>()];
    &KEYS
}

/// The renewal route (state: a [`RouteState`] holding the [`ConsumerSessions`] port).
pub fn routes() -> Router<RouteState> {
    Router::new().route("/api/v1/auth/consumer/renew", post(auth_consumer_renew))
}

/// The default port of an origin's scheme.
fn default_port(scheme: &str) -> u16 {
    if scheme == "https" { 443 } else { 80 }
}

/// Splits `host[:port]` (a bracketed IPv6 literal allowed) into a lowercase host and a port,
/// the scheme's default when none is written. `None` for anything else.
fn host_and_port(authority: &str, default: u16) -> Option<(String, u16)> {
    if authority.is_empty()
        || authority
            .bytes()
            .any(|b| b <= b' ' || matches!(b, b'/' | b'@' | b'?' | b'#' | b'\\' | 0x7f..))
    {
        return None;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (format!("[{inner}]"), port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        }
    };
    if host.is_empty() || host.contains(':') && !host.starts_with('[') {
        return None;
    }
    let port = match port {
        None => default,
        Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p.parse().ok()?,
        Some(_) => return None,
    };
    Some((host.to_ascii_lowercase(), port))
}

/// Whether an `Origin` header value is this site's own origin, given the request's effective host.
/// `https` is the site's scheme; `http` is accepted only for a loopback host (local
/// development), where browsers accept the `Secure` cookie. `Origin: null` and anything that is
/// not exactly `scheme://host[:port]` is foreign. Host and port are compared exactly.
fn origin_is_own(origin: &str, own_host: &str) -> bool {
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    if scheme != "https" && scheme != "http" {
        return false;
    }
    let default = default_port(scheme);
    let (Some((origin_host, origin_port)), Some((own_host, own_port))) = (
        host_and_port(authority, default),
        host_and_port(own_host.trim(), default),
    ) else {
        return false;
    };
    if scheme == "http" && !matches!(origin_host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        return false;
    }
    origin_host == own_host && origin_port == own_port
}

/// Whether the request is one a browser marks as coming from another site: a foreign or
/// unreadable `Origin` (or more than one), or a `Sec-Fetch-Site` that is not one of the four
/// values a browser sends, or is `cross-site`. Neither header present is allowed.
fn cross_site(headers: &HeaderMap, host: Option<&str>) -> bool {
    let mut fetch_site = headers.get_all("sec-fetch-site").iter();
    if let Some(value) = fetch_site.next() {
        if fetch_site.next().is_some() {
            return true;
        }
        match value.to_str().map(str::trim) {
            Ok("same-origin" | "same-site" | "none") => {}
            _ => return true,
        }
    }
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return false;
    };
    if origins.next().is_some() {
        return true;
    }
    let Ok(origin) = origin.to_str() else {
        return true;
    };
    // `host` is the effective host the site middleware decided; the raw `Host` header is never
    // read here. No host (or no `SiteContext`, which gives `None`) is refused.
    !host.is_some_and(|h| origin_is_own(origin, h))
}

/// Longest cookie value read (a real value is 43 characters).
const MAX_COOKIE_VALUE: usize = 256;

/// Whether `value` is non-empty cookie-octets (RFC 6265: no space, `"`, `,`, `;`, `\`, no
/// control or non-ASCII).
fn cookie_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_COOKIE_VALUE
        && value
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b'"' | b',' | b';' | b'\\'))
}

/// The renewal cookie's value, only when it is sent exactly once and is well formed.
fn renewal_cookie(headers: &HeaderMap) -> Option<String> {
    let mut found: Option<&str> = None;
    for header_value in headers.get_all(header::COOKIE) {
        let Ok(text) = header_value.to_str() else {
            continue;
        };
        for pair in text.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name == COOKIE_NAME {
                if found.is_some() {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found.filter(|v| cookie_safe(v)).map(str::to_owned)
}

fn finish(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn failure(error: ApiError, request_id: &RequestId) -> Response {
    finish(render_error(error, request_id))
}

async fn auth_consumer_renew(
    State(state): State<RouteState>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let site = request.extensions().get::<SiteContext>().cloned();
    let request_id = &site
        .as_ref()
        .map(|site| site.request_id.clone())
        .or_else(|| request.extensions().get::<RequestId>().cloned())
        .unwrap_or_else(RequestId::generate);
    // A cross-site request is refused before anything about the cookie is looked at: the answer
    // does not depend on the cookie, and the cookie is neither read nor spent. Without a
    // `SiteContext` there is no effective host to compare, so an `Origin` is refused (fail closed).
    let host = site.as_ref().and_then(|site| site.host.as_deref());
    if cross_site(&headers, host) {
        return failure(
            ApiError::new(Code::PermissionDenied, "This request is not allowed."),
            request_id,
        );
    }
    let Some(site) = site else {
        tracing::error!("missing SiteContext on the consumer renewal route");
        return failure(ApiError::from_code(Code::HttpInternal), request_id);
    };
    let Some(sessions) = state.port::<dyn ConsumerSessions>() else {
        return failure(ApiError::from_code(Code::HttpInternal), request_id);
    };
    // The call has no query: refuse one before the cookie is consumed.
    if request.uri().query().is_some_and(|q| !q.is_empty()) {
        return failure(
            ApiError::new(Code::ApiInvalidRequest, "This call takes no query."),
            request_id,
        );
    }
    let expired = || failure(ApiError::from_code(Code::AuthSessionExpired), request_id);
    let Some(cookie) = renewal_cookie(&headers) else {
        return expired();
    };
    // The bearer is neither needed nor read: the context carries no credential.
    let ctx = RequestCtx {
        site: site.site.clone(),
        credential: None,
        idempotency_key: None,
        language: None,
        request_id: request_id.clone(),
        peer: site.peer,
    };
    let renewal = match sessions.renew(&ctx, &cookie).await {
        Ok(renewal) => renewal,
        Err(RenewError::SessionExpired) => return expired(),
        Err(RenewError::Unavailable) => {
            return failure(ApiError::from_code(Code::ServerUnavailable), request_id);
        }
    };
    // A port that hands back something that cannot be a cookie or a token must not become a
    // header: a failed renewal, nothing set.
    if !cookie_safe(&renewal.next_cookie) || renewal.token.is_empty() {
        tracing::error!("the consumer session port returned a renewal outside the contract");
        return failure(ApiError::from_code(Code::HttpInternal), request_id);
    }
    let set_cookie = format!(
        "{COOKIE_NAME}={}; Max-Age={COOKIE_MAX_AGE_SECS}; Path=/; Secure; HttpOnly; SameSite=Strict",
        renewal.next_cookie
    );
    let Ok(set_cookie) = HeaderValue::from_str(&set_cookie) else {
        return failure(ApiError::from_code(Code::HttpInternal), request_id);
    };
    let body = json!({ "data": { "token": renewal.token, "expires_at": renewal.expires_at } });
    let mut response = (StatusCode::OK, axum::Json(body)).into_response();
    response.headers_mut().insert(header::SET_COOKIE, set_cookie);
    finish(response)
}
