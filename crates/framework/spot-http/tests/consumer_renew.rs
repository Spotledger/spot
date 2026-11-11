//! `auth_consumer_renew` (contract 0.10.0, WP-CON-03.T14): the one-use rotating renewal
//! cookie, replay revoking the family, one answer for every refusal, no secret out of place.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::Value as Json;
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::ports::consumer_sessions::{
    COOKIE_NAME, ConsumerSessions, MemoryConsumerSessions, Renewal,
};
use spot_http::api::router::routes::{RouteState, consumer_renew};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use time::macros::datetime;
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const OTHER_HOST: &str = "erp.beta.com";

struct Fixture {
    router: axum::Router,
    sessions: Arc<MemoryConsumerSessions>,
    clock: Arc<ManualClock>,
}

const PROXY: &str = "10.0.0.7";
const INTERNAL: &str = "internal.local";

fn fixture() -> Fixture {
    fixture_with(Vec::new())
}

fn fixture_with(trusted_proxies: Vec<std::net::IpAddr>) -> Fixture {
    let clock = Arc::new(ManualClock::new(datetime!(2026-10-31 08:00:00 UTC)));
    let sessions = Arc::new(MemoryConsumerSessions::new(
        clock.clone(),
        "test-seed",
        time::Duration::minutes(15),
    ));
    let mut ports = Ports::new();
    ports.provide::<dyn ConsumerSessions>(sessions.clone());
    let state = RouteState::new(ports.subset(consumer_renew::required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let beta = SiteId::new("beta").unwrap();
    let router = ServerBuilder::new(
        ServerConfig {
            trusted_proxies,
            ..ServerConfig::default()
        },
        StaticSites::new()
            .add(HOST, acme.clone())
            .add(INTERNAL, acme.clone())
            .add_site_name(acme)
            .add(OTHER_HOST, beta.clone())
            .add_site_name(beta),
    )
    .site_routes(consumer_renew::routes().with_state(state))
    .build();
    Fixture {
        router,
        sessions,
        clock,
    }
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    raw: String,
    body: Json,
}

async fn send(f: &Fixture, host: &str, uri: &str, headers: &[(&str, String)]) -> Reply {
    send_from(f, None, host, uri, headers).await
}

/// Like [`send`], from a TCP peer (`None`: no connection info, as in the other tests).
async fn send_from(
    f: &Fixture,
    peer: Option<&str>,
    host: &str,
    uri: &str,
    headers: &[(&str, String)],
) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, host);
    for (name, value) in headers {
        builder = builder.header(*name, value.clone());
    }
    let mut request = builder.body(Body::empty()).unwrap();
    if let Some(peer) = peer {
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
                peer.parse().unwrap(),
                40_000,
            )));
    }
    let response = f.router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    Reply {
        status,
        headers,
        body: serde_json::from_str(&raw).unwrap_or(Json::Null),
        raw,
    }
}

fn cookie_header(value: &str) -> (&'static str, String) {
    ("cookie", format!("{COOKIE_NAME}={value}"))
}

async fn renew(f: &Fixture, cookie: &str) -> Reply {
    send(
        f,
        HOST,
        "/api/v1/auth/consumer/renew",
        &[cookie_header(cookie)],
    )
    .await
}

/// The value of the `Set-Cookie` the reply carries.
fn next_cookie(r: &Reply) -> String {
    let set = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    let pair = set.split(';').next().unwrap();
    pair.strip_prefix(&format!("{COOKIE_NAME}="))
        .unwrap()
        .to_owned()
}

fn assert_expired(r: &Reply) {
    assert_eq!(r.status, 401, "{}", r.raw);
    assert_eq!(r.body["error"]["code"], "auth.session_expired", "{}", r.raw);
    assert!(
        r.headers.get(header::SET_COOKIE).is_none(),
        "no Set-Cookie on a failure"
    );
    assert_eq!(r.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
}

#[tokio::test]
async fn a_renewal_returns_a_new_session_and_the_next_cookie_with_exact_attributes() {
    let f = fixture();
    let (cookie, old_token) = f.sessions.sign_in("acme");
    let r = renew(&f, &cookie).await;
    assert_eq!(r.status, 200, "{}", r.raw);
    let data = &r.body["data"];
    let token = data["token"].as_str().unwrap();
    assert!(!token.is_empty() && token != old_token);
    assert_eq!(data["expires_at"], "2026-10-31T08:15:00Z");
    assert_eq!(r.body.as_object().unwrap().len(), 1, "only `data`");
    assert_eq!(
        data.as_object().unwrap().len(),
        2,
        "only token and expires_at"
    );
    assert!(f.sessions.token_is_live("acme", token));

    let set = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert_eq!(
        set,
        format!(
            "{COOKIE_NAME}={}; Max-Age=2592000; Path=/; Secure; HttpOnly; SameSite=Strict",
            next_cookie(&r)
        )
    );
    assert_eq!(r.headers.get_all(header::SET_COOKIE).iter().count(), 1);
    assert_eq!(r.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert!(r.headers.get("x-request-id").is_some());
    // The cookie rotates: the next one is not the one sent.
    assert_ne!(next_cookie(&r), cookie);
    assert!(!set.contains("Domain"));
}

#[tokio::test]
async fn the_chain_keeps_rotating() {
    let f = fixture();
    let (mut cookie, _) = f.sessions.sign_in("acme");
    for _ in 0..4 {
        let r = renew(&f, &cookie).await;
        assert_eq!(r.status, 200, "{}", r.raw);
        cookie = next_cookie(&r);
    }
}

#[tokio::test]
async fn a_replay_revokes_the_whole_family() {
    let f = fixture();
    let (first, first_token) = f.sessions.sign_in("acme");
    let second = renew(&f, &first).await;
    let second_cookie = next_cookie(&second);
    let second_token = second.body["data"]["token"].as_str().unwrap().to_owned();
    assert!(f.sessions.token_is_live("acme", &second_token));

    // The thief replays the used first cookie.
    assert_expired(&renew(&f, &first).await);
    // Every session token of the family ends with it, and so does the honest cookie.
    assert!(!f.sessions.token_is_live("acme", &first_token));
    assert!(!f.sessions.token_is_live("acme", &second_token));
    assert!(f.sessions.family_is_revoked("acme", &second_cookie));
    assert_expired(&renew(&f, &second_cookie).await);
}

#[tokio::test]
async fn a_missing_unknown_malformed_or_repeated_cookie_is_the_same_refusal() {
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    let none = send(&f, HOST, "/api/v1/auth/consumer/renew", &[]).await;
    assert_expired(&none);
    let reference = {
        let mut b = none.body.clone();
        b["error"].as_object_mut().unwrap().remove("request_id");
        b
    };
    let cases: Vec<Vec<(&str, String)>> = vec![
        vec![cookie_header("not-a-real-cookie")],
        vec![cookie_header("")],
        vec![cookie_header(&"a".repeat(300))],
        vec![("cookie", format!("{COOKIE_NAME}=\"{cookie}\""))],
        vec![("cookie", "other=1; theme=dark".to_owned())],
        // The cookie under another name, and the `__Host-`less name, are not it.
        vec![("cookie", format!("spot_consumer_renew={cookie}"))],
        // Sent twice, even with the right value: ambiguous, refused, not consumed.
        vec![(
            "cookie",
            format!("{COOKIE_NAME}={cookie}; {COOKIE_NAME}={cookie}"),
        )],
        vec![cookie_header(&cookie), cookie_header(&cookie)],
    ];
    for (n, headers) in cases.iter().enumerate() {
        let r = send(&f, HOST, "/api/v1/auth/consumer/renew", headers).await;
        assert_expired(&r);
        let mut b = r.body.clone();
        b["error"].as_object_mut().unwrap().remove("request_id");
        assert_eq!(b, reference, "case {n}");
    }
    // None of that consumed the real cookie.
    assert_eq!(renew(&f, &cookie).await.status, 200);
}

#[tokio::test]
async fn an_expired_cookie_is_refused_and_a_live_one_just_inside_the_limit_works() {
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    f.clock
        .advance(std::time::Duration::from_secs(2_592_000 - 1));
    let ok = renew(&f, &cookie).await;
    assert_eq!(ok.status, 200, "{}", ok.raw);
    // A fresh sign-in, then one second past the 30 days.
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    f.clock.advance(std::time::Duration::from_secs(2_592_000));
    assert_expired(&renew(&f, &cookie).await);
}

#[tokio::test]
async fn a_cookie_of_another_site_neither_renews_nor_revokes_anything() {
    let f = fixture();
    let (cookie, token) = f.sessions.sign_in("acme");
    // Sent to the other site's host.
    let r = send(
        &f,
        OTHER_HOST,
        "/api/v1/auth/consumer/renew",
        &[cookie_header(&cookie)],
    )
    .await;
    assert_expired(&r);
    // Acme's family is untouched: its token lives and its cookie still renews.
    assert!(f.sessions.token_is_live("acme", &token));
    assert_eq!(renew(&f, &cookie).await.status, 200);
    // Even a replayed (used) acme cookie sent to beta cannot revoke acme's family.
    let again = send(
        &f,
        OTHER_HOST,
        "/api/v1/auth/consumer/renew",
        &[cookie_header(&cookie)],
    )
    .await;
    assert_expired(&again);
    assert!(!f.sessions.family_is_revoked("acme", &cookie));
}

#[tokio::test]
async fn the_bearer_is_ignored_and_a_query_is_refused_before_the_cookie_is_used() {
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    // A query (a token in a URL) is refused and the cookie survives.
    let r = send(
        &f,
        HOST,
        "/api/v1/auth/consumer/renew?token=abc",
        &[cookie_header(&cookie)],
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.body["error"]["code"], "api.invalid_request");
    assert!(r.headers.get(header::SET_COOKIE).is_none());
    // A bogus bearer neither helps nor hurts.
    let r = send(
        &f,
        HOST,
        "/api/v1/auth/consumer/renew",
        &[
            cookie_header(&cookie),
            ("authorization", "Bearer garbage".to_owned()),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    // And a bearer alone is not a credential here.
    let r = send(
        &f,
        HOST,
        "/api/v1/auth/consumer/renew",
        &[("authorization", "Bearer garbage".to_owned())],
    )
    .await;
    assert_expired(&r);
}

#[tokio::test]
async fn no_secret_appears_in_an_error_body() {
    let f = fixture();
    let (cookie, token) = f.sessions.sign_in("acme");
    let ok = renew(&f, &cookie).await;
    let fresh = next_cookie(&ok);
    let replay = renew(&f, &cookie).await;
    for secret in [&cookie, &token, &fresh] {
        assert!(!replay.raw.contains(secret.as_str()));
    }
    let debug = format!(
        "{:?}",
        Renewal {
            token: "tok-secret".into(),
            expires_at: "2026-10-31T08:15:00Z".into(),
            next_cookie: "cookie-secret".into(),
        }
    );
    assert!(!debug.contains("tok-secret") && !debug.contains("cookie-secret"));
}

/// A port that returns a value that cannot be a cookie must not reach a header.
struct BadPort;
impl ConsumerSessions for BadPort {
    fn renew<'a>(
        &'a self,
        _ctx: &'a spot_http::api::RequestCtx,
        _cookie: &'a str,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        Result<Renewal, spot_http::api::ports::consumer_sessions::RenewError>,
    > {
        Box::pin(async {
            Ok(Renewal {
                token: "t".into(),
                expires_at: "2026-10-31T08:15:00Z".into(),
                next_cookie: "x; Domain=evil.example".into(),
            })
        })
    }
}

#[tokio::test]
async fn a_port_value_that_is_not_cookie_safe_is_an_internal_error_with_nothing_set() {
    let f = fixture();
    let mut ports = Ports::new();
    ports.provide::<dyn ConsumerSessions>(Arc::new(BadPort));
    let state = RouteState::new(ports);
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(consumer_renew::routes().with_state(state))
    .build();
    let g = Fixture {
        router,
        sessions: f.sessions.clone(),
        clock: f.clock.clone(),
    };
    let r = renew(&g, "whatever").await;
    assert_eq!(r.status, 500);
    assert!(r.headers.get(header::SET_COOKIE).is_none());
    assert!(!r.raw.contains("evil.example"));
}

// --- cross-site refusal (WP-PRD-01.T49) -----------------------------------------------------

async fn renew_with(f: &Fixture, cookie: &str, extra: &[(&str, &str)]) -> Reply {
    let mut headers = vec![cookie_header(cookie)];
    for (name, value) in extra {
        headers.push((*name, (*value).to_owned()));
    }
    send(f, HOST, "/api/v1/auth/consumer/renew", &headers).await
}

fn assert_forbidden(r: &Reply) {
    assert_eq!(r.status, 403, "{}", r.raw);
    assert_eq!(r.body["error"]["code"], "permission.denied", "{}", r.raw);
    assert!(
        r.headers.get(header::SET_COOKIE).is_none(),
        "no Set-Cookie on a refusal"
    );
    assert_eq!(r.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
}

#[tokio::test]
async fn a_foreign_origin_is_refused_and_the_same_cookie_still_renews() {
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    let refused = renew_with(&f, &cookie, &[("origin", "https://evil.example")]).await;
    assert_forbidden(&refused);
    // The refusal neither spent nor revoked the cookie, nor its family.
    let ok = renew(&f, &cookie).await;
    assert_eq!(ok.status, 200, "{}", ok.raw);
}

#[tokio::test]
async fn sec_fetch_site_cross_site_is_refused_and_spends_nothing() {
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    let refused = renew_with(&f, &cookie, &[("sec-fetch-site", "cross-site")]).await;
    assert_forbidden(&refused);
    assert_eq!(renew(&f, &cookie).await.status, 200);
}

#[tokio::test]
async fn the_sites_own_origin_renews() {
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    let r = renew_with(
        &f,
        &cookie,
        &[
            ("origin", "https://erp.acme.com"),
            ("sec-fetch-site", "same-origin"),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    // The default port written out is the same origin.
    let r = renew_with(
        &f,
        &next_cookie(&r),
        &[("origin", "https://erp.acme.com:443")],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    // A direct navigation sends `none`.
    let r = renew_with(&f, &next_cookie(&r), &[("sec-fetch-site", "none")]).await;
    assert_eq!(r.status, 200, "{}", r.raw);
}

#[tokio::test]
async fn a_request_without_origin_headers_renews() {
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    assert_eq!(renew(&f, &cookie).await.status, 200);
}

#[tokio::test]
async fn an_origin_that_only_contains_the_sites_host_is_refused() {
    let f = fixture();
    let (cookie, _) = f.sessions.sign_in("acme");
    for origin in [
        "https://erp.acme.com.evil.example",
        "https://evil-erp.acme.com",
        "https://evil.example/erp.acme.com",
        "https://erp.acme.com@evil.example",
        "https://evil.example?erp.acme.com",
        "http://erp.acme.com",
        "https://erp.acme.com:8443",
        "null",
        "erp.acme.com",
    ] {
        let r = renew_with(&f, &cookie, &[("origin", origin)]).await;
        assert_forbidden(&r);
    }
    // Two Origin headers are ambiguous: refused, even when one is the site's own.
    let r = renew_with(
        &f,
        &cookie,
        &[
            ("origin", "https://erp.acme.com"),
            ("origin", "https://evil.example"),
        ],
    )
    .await;
    assert_forbidden(&r);
    // None of the refusals spent the cookie.
    assert_eq!(renew(&f, &cookie).await.status, 200);
}

/// A renewal through a reverse proxy: the proxy rewrites `Host` to the internal name and names
/// the public host in `X-Forwarded-Host`; the browser's `Origin` is the public one.
async fn via_proxy(f: &Fixture, peer: &str, cookie: &str, forwarded: &str, origin: &str) -> Reply {
    send_from(
        f,
        Some(peer),
        INTERNAL,
        "/api/v1/auth/consumer/renew",
        &[
            cookie_header(cookie),
            ("x-forwarded-host", forwarded.to_owned()),
            ("origin", origin.to_owned()),
        ],
    )
    .await
}

#[tokio::test]
async fn behind_a_trusted_proxy_the_forwarded_host_is_the_origin_to_match() {
    let f = fixture_with(vec![PROXY.parse().unwrap()]);
    let (cookie, _) = f.sessions.sign_in("acme");
    // `Host` is the internal name; the public host is forwarded and is the Origin: renews.
    let r = via_proxy(&f, PROXY, &cookie, HOST, "https://erp.acme.com").await;
    assert_eq!(r.status, 200, "{}", r.raw);
    // A port in the forwarded host is compared with the port in the Origin.
    let cookie = next_cookie(&r);
    let r = via_proxy(
        &f,
        PROXY,
        &cookie,
        "erp.acme.com:8443",
        "https://erp.acme.com",
    )
    .await;
    assert_forbidden(&r);
    let r = via_proxy(
        &f,
        PROXY,
        &cookie,
        "erp.acme.com:8443",
        "https://erp.acme.com:8443",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    // A foreign Origin behind the trusted proxy is refused, and spends nothing.
    let cookie = next_cookie(&r);
    let r = via_proxy(&f, PROXY, &cookie, HOST, "https://evil.example").await;
    assert_forbidden(&r);
    let r = via_proxy(&f, PROXY, &cookie, HOST, "https://erp.acme.com").await;
    assert_eq!(r.status, 200, "{}", r.raw);
}

#[tokio::test]
async fn an_untrusted_peers_forwarded_host_is_ignored() {
    let f = fixture_with(vec![PROXY.parse().unwrap()]);
    let (cookie, _) = f.sessions.sign_in("acme");
    // From an untrusted peer the forwarded header is ignored: `Host` is the internal name, the
    // public Origin does not match it and is refused.
    let r = via_proxy(&f, "9.9.9.9", &cookie, HOST, "https://erp.acme.com").await;
    assert_forbidden(&r);
    // Same with no connection information at all.
    let r = send(
        &f,
        INTERNAL,
        "/api/v1/auth/consumer/renew",
        &[
            cookie_header(&cookie),
            ("x-forwarded-host", HOST.to_owned()),
            ("origin", "https://erp.acme.com".to_owned()),
        ],
    )
    .await;
    assert_forbidden(&r);
    // The refused requests spent nothing: the same cookie still renews for a same-site request
    // (the Origin is the internal host the peer really addressed).
    let r = via_proxy(&f, "9.9.9.9", &cookie, HOST, "https://internal.local").await;
    assert_eq!(r.status, 200, "{}", r.raw);
}

#[tokio::test]
async fn a_client_supplied_forwarded_host_entry_is_never_the_origin_to_match() {
    let f = fixture_with(vec![PROXY.parse().unwrap()]);
    let (cookie, _) = f.sessions.sign_in("acme");
    // A proxy that appends: the client's own entry is on the left, the proxy's on the right.
    // The Origin the client chose (its own entry) does not match and is refused.
    let r = via_proxy(
        &f,
        PROXY,
        &cookie,
        "evil.example, erp.acme.com",
        "https://evil.example",
    )
    .await;
    assert_forbidden(&r);
    // The proxy's own entry is the host to match, and nothing was spent.
    let r = via_proxy(
        &f,
        PROXY,
        &cookie,
        "evil.example, erp.acme.com",
        "https://erp.acme.com",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
}
