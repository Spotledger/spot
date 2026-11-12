//! Site resolution (ADR-34): maps a request's host name to its site.

use std::collections::HashMap;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use spot_core::SiteId;

use crate::error::{HttpErrorCode, error_response};
use crate::request::RequestId;

/// Maps a normalized host name to its site. Implementations are state and therefore shared.
pub trait SiteResolver: Send + Sync + std::fmt::Debug + 'static {
    /// Resolves a normalized host to its site. `None` means no site is configured for it.
    fn resolve(&self, host: &str) -> Option<SiteId>;
}

/// A fixed map of host names that map to sites. Empty means every site is disabled.
#[derive(Debug, Default)]
pub struct StaticSites {
    /// Normalized host → site.
    aliases: HashMap<String, SiteId>,
}

impl StaticSites {
    /// Creates an empty map.
    pub fn new() -> Self {
        Self {
            aliases: HashMap::new(),
        }
    }

    /// Adds a host name (normalized on the way in) that resolves to `site`.
    pub fn add(mut self, host: &str, site: SiteId) -> Self {
        self.aliases.insert(normalize_host(host), site);
        self
    }

    /// Adds the site's name itself as a host (the default host of every site).
    pub fn add_site_name(self, site: SiteId) -> Self {
        let name = site.as_str().to_owned();
        self.add(&name, site)
    }
}

impl SiteResolver for StaticSites {
    fn resolve(&self, host: &str) -> Option<SiteId> {
        self.aliases.get(&normalize_host(host)).cloned()
    }
}

/// Host normalization: lower-case, one trailing dot stripped, port stripped for regular hosts
/// (IPv6 keeps its brackets and is not port-stripped). The empty string stands for a missing
/// or empty `Host` header.
pub fn normalize_host(raw: &str) -> String {
    let host = raw.trim().to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    let host = if host.starts_with('[') {
        match host.find(']') {
            Some(end) => &host[..=end],
            None => "",
        }
    } else {
        host.split(':').next().unwrap_or("")
    };
    host.to_owned()
}

/// The `X-Forwarded-Host` entry a trusted proxy added, but only when the connection's peer is a
/// trusted proxy. Raw forwarded headers on a non-proxy connection are untrusted and ignored.
/// All header lines are read as one comma-joined list and walked from the right, like
/// [`client_addr`] walks `X-Forwarded-For`: a proxy that appends puts its own value last, so the
/// entries to the left of it are client-supplied and never reached. The answer is the rightmost
/// non-empty entry. A line that is not valid UTF-8 cannot be vouched for: no forwarded host.
pub(crate) fn forwarded_host(request: &Request, trusted_proxies: &[IpAddr]) -> Option<String> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0);
    // The same trust test as `client_addr`: an IPv4-mapped IPv6 peer is its IPv4 address.
    let peer_is_trusted = peer.is_some_and(|peer| {
        let ip = peer.ip().to_canonical();
        trusted_proxies
            .iter()
            .any(|trusted| trusted.to_canonical() == ip)
    });
    if !peer_is_trusted {
        return None;
    }
    let mut lines = Vec::new();
    for value in request.headers().get_all("x-forwarded-host") {
        lines.push(value.to_str().ok()?);
    }
    lines
        .iter()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .rev()
        .find(|entry| !entry.is_empty())
        .map(str::to_owned)
}

/// The host a request is served for, raw `host[:port]` as received: the rightmost non-empty
/// `X-Forwarded-Host` entry when the connection's peer is a trusted proxy, else the `Host`
/// header, else the request URI's authority (HTTP/2 `:authority`). More than one `Host` header
/// is ambiguous and gives `None`. `None` when there is none.
/// The one place the effective host is decided; [`SiteContext::host`] carries the answer.
pub(crate) fn effective_host(request: &Request, trusted_proxies: &[IpAddr]) -> Option<String> {
    forwarded_host(request, trusted_proxies).or_else(|| {
        let mut hosts = request.headers().get_all("host").iter();
        match (hosts.next(), hosts.next()) {
            (Some(_), Some(_)) => None,
            (Some(value), None) => value
                .to_str()
                .ok()
                .map(str::trim)
                .filter(|raw| !raw.is_empty())
                .map(str::to_owned),
            (None, _) => request.uri().authority().map(|a| a.as_str().to_owned()),
        }
    })
}

/// The address a request is attributed to (the throttle source). It starts from the TCP
/// peer. Only when that peer is a trusted proxy is `X-Forwarded-For` consulted: the list (all
/// header lines read as one comma-joined list) is walked from the right, skipping entries that
/// are themselves trusted proxies, and the first other entry is the answer. Entries to the
/// left of it are attacker-controlled and never reached. Anything unusable (no header, an
/// empty or unparsable entry, a list of only trusted proxies) yields the connection address;
/// a header is never believed on its own. IPv4-mapped IPv6 addresses are normalised to IPv4.
pub(crate) fn client_addr(
    connection: Option<IpAddr>,
    forwarded_for: &[&str],
    trusted_proxies: &[IpAddr],
) -> Option<IpAddr> {
    let connection = connection.map(|ip| ip.to_canonical());
    let is_trusted = |ip: IpAddr| {
        trusted_proxies
            .iter()
            .any(|trusted| trusted.to_canonical() == ip)
    };
    let Some(conn) = connection.filter(|ip| is_trusted(*ip)) else {
        return connection;
    };
    let rightmost_untrusted = forwarded_for
        .iter()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .rev()
        .map(|entry| entry.parse::<IpAddr>().map(|ip| ip.to_canonical()))
        .find(|entry| !entry.as_ref().is_ok_and(|ip| is_trusted(*ip)));
    match rightmost_untrusted {
        Some(Ok(ip)) => Some(ip),
        _ => Some(conn),
    }
}

/// [`client_addr`] for a request: reads the connection address and every `X-Forwarded-For`
/// line.
pub(crate) fn request_client_addr(request: &Request, trusted_proxies: &[IpAddr]) -> Option<IpAddr> {
    let connection = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0.ip());
    // A non-UTF-8 line cannot be vouched for: it becomes an unparsable entry.
    let lines: Vec<&str> = request
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .map(|value| value.to_str().unwrap_or("?"))
        .collect();
    client_addr(connection, &lines, trusted_proxies)
}

/// The site resolution state shared by every request.
#[derive(Debug)]
pub(crate) struct SiteResolution {
    /// The resolver configured for this server.
    pub(crate) resolver: Arc<dyn SiteResolver>,
    /// Proxy peers whose forwarded host is honored.
    pub(crate) trusted_proxies: Arc<[IpAddr]>,
    /// Site served when no host resolves.
    pub(crate) fallback_site: Option<SiteId>,
}

/// Resolves the request's site before routing. Inserts a [`SiteContext`] on success; responds
/// with the `http.unknown_site` envelope on failure.
pub(crate) async fn resolve_site_middleware(
    axum::extract::State(resolution): axum::extract::State<Arc<SiteResolution>>,
    request: Request,
    next: Next,
) -> Response {
    let host = effective_host(&request, &resolution.trusted_proxies);
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let peer = request_client_addr(&request, &resolution.trusted_proxies);
    let site = host
        .as_deref()
        .map(normalize_host)
        .filter(|host| !host.is_empty())
        .and_then(|host| resolution.resolver.resolve(&host))
        .or_else(|| resolution.fallback_site.clone());
    match site {
        Some(site) => {
            tracing::trace!(site = %site, "resolved site");
            tracing::Span::current().record("site", site.as_str());
            let mut request = request;
            request.extensions_mut().insert(SiteContext {
                site,
                request_id,
                peer,
                host: host.clone(),
            });
            next.run(request).await
        }
        None => {
            let display_host = host
                .as_deref()
                .map(normalize_host)
                .filter(|host| !host.is_empty())
                .unwrap_or_else(|| "no host".into());
            error_response(
                HttpErrorCode::UnknownSite,
                format!("No site is configured for host \"{display_host}\"."),
                Some(&request_id),
            )
        }
    }
}

/// Site of the current request plus its id, for site-route handlers.
#[derive(Clone, Debug)]
pub struct SiteContext {
    /// The site the request resolves to.
    pub site: SiteId,
    /// The request id.
    pub request_id: RequestId,
    /// The address the request is attributed to: the connection, or the forwarded client when
    /// the connection is a trusted proxy. `None` when unknown.
    pub peer: Option<IpAddr>,
    /// The effective host the request is served for, raw `host[:port]` as received (not
    /// normalized: the port matters when it is compared with an `Origin`). It is the rightmost
    /// non-empty `X-Forwarded-Host` entry only when the connection is from a trusted proxy,
    /// else the `Host` header (or the URI authority). `None` when the request names no host.
    pub host: Option<String>,
}

/// Rejection: a 500 envelope (nothing else can go wrong here).
pub struct MissingSiteContext;

impl IntoResponse for MissingSiteContext {
    fn into_response(self) -> Response {
        tracing::error!("missing SiteContext on a site route");
        error_response(
            HttpErrorCode::Internal,
            HttpErrorCode::Internal.default_message(),
            None,
        )
    }
}

impl<S> FromRequestParts<S> for SiteContext
where
    S: Send + Sync,
{
    type Rejection = MissingSiteContext;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SiteContext>()
            .cloned()
            .ok_or(MissingSiteContext)
    }
}

/// The matched route template for a log line, or the unmatched marker. Never the concrete path,
/// which may carry a secret on an open route.
fn matched_route(request: &Request) -> &str {
    request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or(spot_telemetry::UNMATCHED_ROUTE, |matched| matched.as_str())
}

/// Fallback for a path no site route serves: `api.unknown_route`.
pub async fn unknown_route_fallback(request: Request) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    tracing::debug!(
        request_id = %request_id,
        route = matched_route(&request),
        "unknown route",
    );
    error_response(
        HttpErrorCode::UnknownRoute,
        HttpErrorCode::UnknownRoute.default_message(),
        Some(&request_id),
    )
}

/// Fallback for a known path with an unsupported method: `api.method_not_allowed`.
pub async fn method_not_allowed_fallback(request: Request) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    tracing::debug!(
        request_id = %request_id,
        method = %request.method(),
        route = matched_route(&request),
        "method not allowed",
    );
    error_response(
        HttpErrorCode::MethodNotAllowed,
        HttpErrorCode::MethodNotAllowed.default_message(),
        Some(&request_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Must;
    use spot_core::SiteId;

    fn host_request(peer: &str, host: Option<&str>, forwarded: &[&str]) -> Request {
        let mut builder = axum::http::Request::builder().uri("/");
        if let Some(host) = host {
            builder = builder.header("host", host);
        }
        for line in forwarded {
            builder = builder.header("x-forwarded-host", *line);
        }
        let mut request = builder.body(axum::body::Body::empty()).must();
        let addr = SocketAddr::new(peer.parse().must(), 40_000);
        request.extensions_mut().insert(ConnectInfo(addr));
        request
    }

    #[test]
    fn effective_host_table() {
        let trusted: Vec<IpAddr> = vec!["10.0.0.1".parse().must()];
        let host =
            |peer, host, fwd: &[&str]| effective_host(&host_request(peer, host, fwd), &trusted);
        let s = |v: &str| Some(v.to_owned());
        // A trusted peer's forwarded host wins, raw (port kept, case kept).
        assert_eq!(
            host("10.0.0.1", Some("internal:8080"), &["Public.Example:8443"]),
            s("Public.Example:8443")
        );
        // The same header from an untrusted peer is ignored.
        assert_eq!(
            host("9.9.9.9", Some("internal:8080"), &["public.example"]),
            s("internal:8080")
        );
        // A trusted peer without the header: Host.
        assert_eq!(host("10.0.0.1", Some("internal"), &[]), s("internal"));
        // The rightmost non-empty entry: the one the trusted proxy appended, never the
        // client-supplied ones to its left (comma lists and several header lines).
        assert_eq!(
            host(
                "10.0.0.1",
                Some("internal"),
                &["evil.example, good.example, "]
            ),
            s("good.example")
        );
        assert_eq!(
            host(
                "10.0.0.1",
                Some("internal"),
                &["evil.example", "good.example"]
            ),
            s("good.example")
        );
        // A forwarded line that is not valid UTF-8 is not believed: Host.
        let mut odd = host_request("10.0.0.1", Some("internal"), &[]);
        odd.headers_mut().append(
            "x-forwarded-host",
            axum::http::HeaderValue::from_bytes(b"caf\xe9.example").must(),
        );
        assert_eq!(effective_host(&odd, &trusted), s("internal"));
        // Two Host headers are ambiguous: no host (the renewal route then fails closed).
        let mut twice = host_request("9.9.9.9", Some("a.example"), &[]);
        twice
            .headers_mut()
            .append("host", axum::http::HeaderValue::from_static("b.example"));
        assert_eq!(effective_host(&twice, &trusted), None);
        // An empty forwarded header falls back to Host.
        assert_eq!(host("10.0.0.1", Some("internal"), &[" , "]), s("internal"));
        // An IPv4-mapped trusted peer is trusted, like in `client_addr`.
        assert_eq!(
            host("::ffff:10.0.0.1", Some("internal"), &["public.example"]),
            s("public.example")
        );
        // No host at all.
        assert_eq!(host("9.9.9.9", None, &[]), None);
    }

    #[test]
    fn client_addr_table() {
        let ip = |s: &str| s.parse::<IpAddr>().ok();
        let proxy = "10.0.0.1";
        let trusted: Vec<IpAddr> = vec!["10.0.0.1".parse().must(), "10.0.0.2".parse().must()];
        // (name, connection, X-Forwarded-For lines, expected address)
        type Case<'a> = (&'a str, Option<&'a str>, &'a [&'a str], Option<&'a str>);
        let cases: &[Case] = &[
            (
                "untrusted forged",
                Some("9.9.9.9"),
                &["1.1.1.1"],
                Some("9.9.9.9"),
            ),
            ("one entry", Some(proxy), &["1.1.1.1"], Some("1.1.1.1")),
            (
                "evil, real",
                Some(proxy),
                &["6.6.6.6, 1.1.1.1"],
                Some("1.1.1.1"),
            ),
            (
                "real, proxy2",
                Some(proxy),
                &["1.1.1.1, 10.0.0.2"],
                Some("1.1.1.1"),
            ),
            (
                "all trusted",
                Some(proxy),
                &["10.0.0.2, 10.0.0.1"],
                Some(proxy),
            ),
            ("garbage", Some(proxy), &["not-an-ip"], Some(proxy)),
            (
                "garbage after real",
                Some(proxy),
                &["1.1.1.1, junk"],
                Some(proxy),
            ),
            ("empty header", Some(proxy), &[""], Some(proxy)),
            ("no header", Some(proxy), &[], Some(proxy)),
            ("ipv6", Some(proxy), &["2001:db8::1"], Some("2001:db8::1")),
            (
                "v4-mapped",
                Some(proxy),
                &["::ffff:1.2.3.4"],
                Some("1.2.3.4"),
            ),
            (
                "v4-mapped proxy skipped",
                Some(proxy),
                &["1.1.1.1, ::ffff:10.0.0.2"],
                Some("1.1.1.1"),
            ),
            (
                "mapped connection",
                Some("::ffff:10.0.0.1"),
                &["1.1.1.1"],
                Some("1.1.1.1"),
            ),
            (
                "two lines",
                Some(proxy),
                &["6.6.6.6", "1.1.1.1, 10.0.0.2"],
                Some("1.1.1.1"),
            ),
            ("no connection", None, &["1.1.1.1"], None),
        ];
        for (name, conn, lines, want) in cases {
            let got = client_addr(conn.and_then(ip), lines, &trusted);
            assert_eq!(got, want.and_then(ip), "{name}");
        }
    }

    #[test]
    fn normalizes_case_and_port_and_trailing_dot() {
        assert_eq!(normalize_host("ACME.spotledger.App"), "acme.spotledger.app");
        assert_eq!(
            normalize_host("acme.spotledger.app:8080"),
            "acme.spotledger.app"
        );
        assert_eq!(
            normalize_host("acme.spotledger.app."),
            "acme.spotledger.app"
        );
        assert_eq!(normalize_host("localHost"), "localhost");
    }

    #[test]
    fn normalizes_ipv6_with_brackets() {
        assert_eq!(normalize_host("[::1]:8080"), "[::1]");
        assert_eq!(normalize_host("[2001:DB8::1]"), "[2001:db8::1]");
    }

    #[test]
    fn empty_host_normalizes_to_empty() {
        assert_eq!(normalize_host(""), "");
        assert_eq!(normalize_host("   "), "");
    }

    #[test]
    fn resolver_matches_normalized_hosts_only() {
        let site = SiteId::new("acme").must();
        let resolver = StaticSites::new()
            .add("Acme.spotledger.app:8443", site.clone())
            .add_site_name(SiteId::new("demo").must());
        assert_eq!(resolver.resolve("acme.spotledger.app"), Some(site));
        assert_eq!(resolver.resolve("demo"), Some(SiteId::new("demo").must()));
        assert_eq!(resolver.resolve("unknown.spotledger.app"), None);
    }

    #[test]
    fn resolver_ignores_port_when_comparing() {
        let site = SiteId::new("acme").must();
        let resolver = StaticSites::new().add("acme.spotledger.app", site.clone());
        assert_eq!(resolver.resolve("acme.spotledger.app:443"), Some(site));
    }
}
