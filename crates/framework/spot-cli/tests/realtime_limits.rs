//! The realtime cap and its metrics (WP-PLT-08.T7). The settings cases need no database; the
//! site cases use PostgreSQL through `SPOT_TEST_PG_URL` and skip without it, like `realtime.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use spot_cli::config::Realtime;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn the_cap_defaults_to_5000_and_unknown_keys_are_refused() {
    assert_eq!(Realtime::default().max_connections, 5000);
    let parsed: Realtime = toml::from_str("max_connections = 7").unwrap();
    assert_eq!(parsed.max_connections, 7);
    assert!(toml::from_str::<Realtime>("max_connection = 7").is_err());
}

/// A raw upgrade request; returns the response head (status line and headers) and the body so far.
async fn raw_upgrade(addr: std::net::SocketAddr, host: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET /api/v1/realtime HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: spot.v1\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = vec![0_u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_with_max_connections_two_refuses_the_third_socket_with_429() {
    let Some(env) = Env::new("[services.realtime]\nmax_connections = 2\n").await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let _ = env.site();
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    // Two sockets that never authenticate hold the two slots.
    let _a = Ws::connect(addr, &host).await;
    let _b = Ws::connect(addr, &host).await;
    let head = raw_upgrade(addr, &host).await;
    assert!(head.starts_with("HTTP/1.1 429"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("retry-after: 5"),
        "{head}"
    );
    assert!(head.contains("realtime.too_many_connections"), "{head}");
    // The refusal is counted; /metrics (when served) names the gauges.
    let snapshot = spot_realtime::ws::limits::counters().snapshot();
    assert!(snapshot.rejected_cap >= 1);
}
