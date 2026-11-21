//! The cap on open sockets (WP-PLT-08.T7): the real router over a loopback listener.
//!
//! The harness of `support` always builds the route without limits, so this suite assembles its
//! own server with `router_with` and reuses the client and the contract helpers.

#[allow(dead_code)]
mod support;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use spot_core::{SiteId, SystemClock};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_realtime::ws::limits::{ConnectionGate, counters};
use spot_realtime::ws::{Rejected, TokenVerifier, WsConfig, WsLimits, router_with};
use spot_realtime::{AllowAll, BusConfig, Principal, RealtimeBus};
use support::{Must, SUBPROTOCOL, TEST_TOKEN, TEST_USER};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;

/// Admits `TEST_TOKEN` as `TEST_USER`, nothing else.
struct OneUser;

impl TokenVerifier for OneUser {
    fn verify<'a>(
        &'a self,
        _site: &'a SiteId,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Principal, Rejected>> + Send + 'a>> {
        Box::pin(async move {
            if token == TEST_TOKEN {
                Ok(Principal::user(TEST_USER.to_owned()))
            } else {
                Err(Rejected::new("no such session"))
            }
        })
    }
}

struct Server {
    addr: std::net::SocketAddr,
    gate: ConnectionGate,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(max: Option<usize>) -> Server {
    let site = SiteId::new("acme.example").must();
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let limits = WsLimits::with_max_connections(max);
    let gate = limits.gate.clone();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add("127.0.0.1", site.clone())
            .add_site_name(site),
    )
    .site_routes(router_with(
        bus,
        Arc::new(OneUser),
        WsConfig::default(),
        limits,
    ))
    .build();
    let listener = TcpListener::bind("127.0.0.1:0").await.must();
    let addr = listener.local_addr().must();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .unwrap_or_else(|e| panic!("server failed: {e}"));
    });
    Server { addr, gate, task }
}

/// Waits (on the test clock) until `gate` shows `want` open sockets.
async fn until_open(gate: &ConnectionGate, want: usize) {
    for _ in 0..600 {
        if gate.open() == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(gate.open(), want, "the gate never reached {want}");
}

#[tokio::test]
async fn the_fourth_socket_past_a_cap_of_three_gets_429_with_the_envelope() {
    let server = server(Some(3)).await;
    let before = counters().snapshot().rejected_cap;
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(support::connect(server.addr).await);
    }
    assert_eq!(server.gate.open(), 3);
    let refused = support::connect_raw(server.addr, Some(SUBPROTOCOL)).await;
    let Err(tungstenite::Error::Http(response)) = refused else {
        panic!("the fourth upgrade must be refused with an HTTP response");
    };
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("5")
    );
    let body: serde_json::Value =
        serde_json::from_slice(response.body().as_deref().unwrap_or_default()).must();
    assert_eq!(body["error"]["code"], "realtime.too_many_connections");
    assert_eq!(body["error"]["retryable"], true);
    assert!(body["error"]["request_id"].is_string());
    assert!(counters().snapshot().rejected_cap > before);
    drop(held);
}

#[tokio::test]
async fn closing_a_socket_frees_its_slot() {
    let server = server(Some(2)).await;
    let a = support::connect(server.addr).await;
    let _b = support::connect(server.addr).await;
    assert!(
        support::connect_raw(server.addr, Some(SUBPROTOCOL))
            .await
            .is_err()
    );
    a.close(None).await;
    until_open(&server.gate, 1).await;
    let _c = support::connect(server.addr).await;
    assert_eq!(server.gate.open(), 2);
}

#[tokio::test(start_paused = true)]
async fn sockets_that_never_authenticate_count_and_free_their_slot_at_the_auth_deadline() {
    let server = server(Some(2)).await;
    let first = support::connect(server.addr).await;
    let second = support::connect(server.addr).await;
    assert_eq!(server.gate.open(), 2, "unauthenticated sockets count");
    assert!(
        support::connect_raw(server.addr, Some(SUBPROTOCOL))
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(first.read_until_close().await, 4001);
    assert_eq!(second.read_until_close().await, 4001);
    until_open(&server.gate, 0).await;
    let _again = support::connect(server.addr).await;
}

#[tokio::test]
async fn a_failed_upgrade_does_not_leak_a_slot() {
    let server = server(Some(1)).await;
    // No subprotocol: refused with 400 before any slot is taken.
    let refused = support::connect_raw(server.addr, None).await;
    assert!(refused.is_err());
    assert_eq!(server.gate.open(), 0);
    let _ok = support::connect(server.addr).await;
    assert_eq!(server.gate.open(), 1);
}

#[tokio::test]
async fn two_hundred_connect_and_disconnect_cycles_leave_the_count_at_zero() {
    let server = server(Some(5)).await;
    for _ in 0..200 {
        let client = support::connect(server.addr).await;
        client.close(None).await;
    }
    until_open(&server.gate, 0).await;
}

#[tokio::test]
async fn no_cap_means_unlimited_and_the_default_limits_change_nothing() {
    let server = server(None).await;
    let mut held = Vec::new();
    for _ in 0..12 {
        held.push(support::connect(server.addr).await);
    }
    assert_eq!(server.gate.open(), 12);
    assert_eq!(WsLimits::default().gate.max(), None);
    drop(held);
}
