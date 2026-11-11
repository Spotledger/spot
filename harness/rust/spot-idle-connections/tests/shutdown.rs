//! The server's own connection count and the realtime bus's subscription count agree
//! (WP-PLT-01.T3): `ServerHandle::open_connections()` counts upgraded WebSockets, so a
//! deployment can export it as its connection gauge instead of reading the bus, and both go
//! back to zero on shutdown.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use spot_idle_connections::RunConfig;
use spot_idle_connections::client::{connect, http1_get, ws_join};
use spot_idle_connections::server::{self, LIVE_PATH, room_for, token_for};

async fn settle(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_server_count_includes_websockets_and_matches_the_bus() {
    let config = RunConfig {
        shutdown_grace: Duration::from_secs(5),
        ..RunConfig::default()
    };
    let mut running = server::start(&config).await.unwrap();
    let addr = running.addr;

    // Twenty authenticated, joined WebSockets and three idle HTTP/1.1 connections.
    let mut sockets = Vec::new();
    for n in 0..20 {
        sockets.push(ws_join(addr, &token_for(n), &room_for(n)).await.unwrap());
    }
    let mut http = Vec::new();
    for _ in 0..3 {
        let mut stream = connect(addr).await.unwrap();
        assert_eq!(http1_get(&mut stream, addr, LIVE_PATH).await.unwrap(), 200);
        http.push(stream);
    }

    settle("the bus sees every WebSocket", || {
        running.ws_sessions() == 20
    })
    .await;
    settle("the server counts every connection", || {
        running.handle.open_connections() == 23
    })
    .await;
    assert_eq!(
        running.handle.open_connections(),
        running.ws_sessions() + http.len(),
        "the server's count is the bus's WebSockets plus the plain HTTP connections"
    );

    // Shutdown: both counts return to zero, and the join finishes.
    running.begin_shutdown();
    settle("both counts reach zero", || {
        running.handle.open_connections() == 0 && running.ws_sessions() == 0
    })
    .await;
    tokio::time::timeout(Duration::from_secs(10), running.handle.join())
        .await
        .unwrap()
        .unwrap();
    drop((sockets, http));
}
