//! WP-PLT-02.T6: the request context's `peer` through the real router. A forwarded address is
//! believed only when the TCP peer is a configured trusted proxy.

#[allow(dead_code)]
mod support;

use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use support::Must;
use support::fake::TOKEN_ALICE;
use support::harness::App;
use tower::ServiceExt;

const PROXY: &str = "10.0.0.7";

fn app() -> App {
    let config = spot_http::ServerConfig {
        trusted_proxies: vec![PROXY.parse().must()],
        ..spot_http::ServerConfig::default()
    };
    App::build(config)
}

/// Sends a `GET` for one customer (a route that builds its context with `build_ctx`; the
/// document does not exist, which does not matter) and returns the peer the service saw.
async fn seen_by_resource(from: &str, forwarded: &[&str]) -> Option<IpAddr> {
    let app = app();
    let mut builder = Request::builder()
        .method("GET")
        .uri("/api/v1/resource/customer/01900000-0000-7000-8000-000000000001")
        .header("host", support::harness::HOST)
        .header("authorization", format!("Bearer {TOKEN_ALICE}"));
    for line in forwarded {
        builder = builder.header("x-forwarded-for", *line);
    }
    let mut request = builder.body(Body::empty()).must();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(from.parse().must(), 40_000)));
    let _ = app.router.clone().oneshot(request).await.must();
    app.fake.seen_peers().last().copied().flatten()
}

fn ip(text: &str) -> Option<IpAddr> {
    text.parse().ok()
}

#[tokio::test]
async fn build_ctx_attributes_the_request_to_the_right_address() {
    // A forged header from an ordinary connection is ignored.
    assert_eq!(
        seen_by_resource("203.0.113.9", &["1.2.3.4"]).await,
        ip("203.0.113.9")
    );
    // A trusted proxy vouches for the entry it appended; the attacker's prefix is never reached.
    assert_eq!(
        seen_by_resource(PROXY, &["6.6.6.6, 198.51.100.4"]).await,
        ip("198.51.100.4")
    );
    // Several header lines are one list.
    assert_eq!(
        seen_by_resource(PROXY, &["6.6.6.6", "198.51.100.4"]).await,
        ip("198.51.100.4")
    );
    // Garbage falls back to the connection.
    assert_eq!(seen_by_resource(PROXY, &["junk"]).await, ip(PROXY));
    // No header: the connection.
    assert_eq!(seen_by_resource(PROXY, &[]).await, ip(PROXY));
}

#[tokio::test]
async fn login_context_carries_the_peer_too() {
    let app = app();
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header("host", support::harness::HOST)
        .header("content-type", "application/json")
        .header("x-forwarded-for", "198.51.100.4")
        .body(Body::from(
            r#"{"user":"alice@acme.com","password":"correct horse"}"#,
        ))
        .must();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(PROXY.parse().must(), 40_000)));
    let response = app.router.clone().oneshot(request).await.must();
    assert_eq!(response.status(), 200);
    assert_eq!(app.fake.seen_peers(), vec![ip("198.51.100.4")]);
}
