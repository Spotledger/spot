//! Liveness (suite brief WP-PLT-08.T2): the heartbeat ping, the idle close, the slow
//! consumer disconnection with ten thousand events, the server-shutdown close, and the
//! session-expiry close through the verifier's watch. Every server message read is validated
//! against the contract's `openapi.yaml`.
//!
//! The two heartbeat/idleness rounds below run on the real wall clock, not the paused
//! clock: a client that only answers pings lives off real timer firings, and the paused
//! clock's auto-advance (it free-runs whenever the runtime has nothing runnable — which a
//! blocking pool read inside `support::Client` leaves it with) makes a paused-time idleness
//! close impossible to stage deterministically behind a real socket.

#[allow(dead_code)]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use support::{TEST_TOKEN, connect_and_auth, doc_room_name, expect_kind, harness, start};
use tokio_tungstenite::tungstenite::Message as WireMessage;

/// A client that only answers our heartbeat pings — the automatic pong a browser's stack
/// answers them with, and the only sign a quiet tab gives — stays connected: read the
/// heartbeat for several periods past the idle window, then speak the protocol and get the
/// answer (real wall clock; the contract's idleness close must not mistake pongs for
/// silence).
#[tokio::test(start_paused = false)]
async fn a_client_that_answers_only_the_heartbeats_is_never_idle_closed() {
    let mut harness = start(1, Arc::new(spot_realtime::AllowAll), 256).await;
    harness.admit(TEST_TOKEN, support::TEST_USER);
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    // Read (and by that answer) the heartbeat past the 2 × heartbeat idle window: the
    // automatic pongs are liveness after the auth.
    let heartbeat = Duration::from_secs(1);
    let answered = tokio::time::timeout(4 * heartbeat + Duration::from_secs(5), async {
        let began = Instant::now();
        while began.elapsed() < 3 * heartbeat {
            if let WireMessage::Ping(_) = client.read().await {
                // ... the client's stack answered with a pong on the socket
            }
        }
        true
    })
    .await;
    assert!(answered.is_ok(), "the heartbeats must keep coming");
    // The connection is well past a silent connection's idle window; the protocol still
    // answers on it.
    client.send_message(&support::ClientMessage::Ping).await;
    let pong = expect_kind(&client, "pong").await;
    assert_eq!(pong["type"], "pong");
}

#[tokio::test(start_paused = true)]
async fn the_heartbeat_pings_at_its_cadence_while_the_client_speaks() {
    // (The ping cadence itself, separate from idleness: the client speaks, so no idle close
    // could ever race the assertion.)
    let mut harness = support::start(1, std::sync::Arc::new(spot_realtime::AllowAll), 256).await;
    harness.admit(TEST_TOKEN, support::TEST_USER);
    let (client, ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    assert_eq!(ready["heartbeat_seconds"], 1);
    // A silent client gets its heartbeat ping once a period. Paused clock: the close cannot
    // fire behind the parked read below, so this is the cadence alone.
    tokio::time::advance(Duration::from_millis(1_200)).await;
    let frame = client.read().await;
    assert!(
        matches!(frame, WireMessage::Ping(_)),
        "expected a heartbeat ping, got {frame:?}"
    );
}

/// The contract's idle close: the client sends nothing at all (its unread heartbeat pings
/// sit in its queue; its automatic pongs never fire), and the server closes with the plain
/// normal close at 2 × `heartbeat_seconds` (real wall clock; see the suite header).
#[tokio::test(start_paused = false)]
async fn a_connection_that_sends_nothing_for_two_heartbeats_is_closed() {
    let mut harness = start(1, Arc::new(spot_realtime::AllowAll), 256).await;
    harness.admit(TEST_TOKEN, support::TEST_USER);
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    // Past the idle window without reading anything (nothing read, nothing sent). The read
    // still happens inside the server's close linger — a grace second that keeps the socket
    // readable after the close frame, exactly so a client can observe the code before the
    // socket is dropped (its pending pong would otherwise be the last thing the wire saw).
    tokio::time::sleep(Duration::from_millis(2_050)).await;
    assert_eq!(client.read_until_close().await, 1000);
}

#[tokio::test(start_paused = false)]
async fn a_client_that_keeps_speaking_is_never_idle_closed() {
    // Speaking (an app-level ping) resets the idle clock; with a real heartbeat and the
    // real clock this runs well past a silent connection's idle window.
    let mut harness = start(1, Arc::new(spot_realtime::AllowAll), 256).await;
    harness.admit(TEST_TOKEN, support::TEST_USER);
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(900)).await;
        client.send_message(&support::ClientMessage::Ping).await;
        let pong = expect_kind(&client, "pong").await;
        assert_eq!(pong["type"], "pong");
    }
    // (Slightly past the 2 s idle window from the last app-ping: it reset the clock.)
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.send_message(&support::ClientMessage::Ping).await;
    let pong = expect_kind(&client, "pong").await;
    assert_eq!(pong["type"], "pong");
}

#[tokio::test]
async fn a_client_that_never_reads_while_ten_thousand_events_are_published_closes_4008() {
    let mut harness = support::start(25, std::sync::Arc::new(spot_realtime::AllowAll), 256).await;
    harness.admit(TEST_TOKEN, support::TEST_USER);
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    let room = doc_room_name(1);
    client
        .send_message(&support::ClientMessage::Join {
            client_ref: "j1".to_owned(),
            room: room.clone(),
            resume_after: None,
        })
        .await;
    expect_kind(&client, "joined").await;
    let room_room = spot_realtime::Room::parse(&room).unwrap_or_else(|e| panic!("room: {e}"));
    // Ten thousand publishes, while nobody reads: the bus disconnects the client rather than
    // waiting for it (ADR-25).
    for i in 1..=10_000u64 {
        harness.bus.publish(
            &harness.site,
            &room_room,
            "doc_updated",
            serde_json::json!({ "doctype": "items", "id": "x1", "version": i }),
        );
    }
    // The client reads what had been streamed and then the close code for a slow consumer.
    assert_eq!(client.read_until_close().await, 4008);
}

#[tokio::test]
async fn server_shutdown_closes_the_connection_with_1001() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    let room = doc_room_name(1);
    client
        .send_message(&support::ClientMessage::Join {
            client_ref: "j1".to_owned(),
            room: room.clone(),
            resume_after: None,
        })
        .await;
    expect_kind(&client, "joined").await;
    // The server is draining: connections close with 1001 so the client reconnects (with
    // resume) after a jittered second or five.
    harness.shutdown.cancel();
    assert_eq!(client.read_until_close().await, 1001);
}

#[tokio::test(start_paused = true)]
async fn server_shutdown_reaches_even_a_connection_missing_its_auth_window() {
    // Shutdown wins over the handshake: a pre-auth connection closes 1001, not 4001, when
    // the server is drained first.
    let mut harness = support::start(25, std::sync::Arc::new(spot_realtime::AllowAll), 256).await;
    harness.admit(TEST_TOKEN, support::TEST_USER);
    let client = support::connect(harness.addr).await;
    harness.shutdown.cancel();
    // Paused time: no wall-clock deadline can fire first.
    assert_eq!(client.read_until_close().await, 1001);
}

#[tokio::test]
async fn session_expiry_closes_4009_when_the_verifier_watches() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    // The session dies under the connection; the verifier's watch sees the revocation, the
    // transport re-verifies (fails now), and the contract's 4009 closes the socket.
    harness.revoke(TEST_TOKEN);
    assert_eq!(client.read_until_close().await, 4009);
}

#[tokio::test]
async fn a_revocation_signal_that_leaves_the_session_valid_does_not_close() {
    let mut harness = support::start(25, std::sync::Arc::new(spot_realtime::AllowAll), 256).await;
    harness.admit(TEST_TOKEN, support::TEST_USER);
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    // The watch resolves (a signal without a removal — how a tolerated permission change
    // reaches the transport); the watcher re-verifies, succeeds, and keeps watching. The
    // connection must stay open.
    harness.signal(TEST_TOKEN);
    client.send_message(&support::ClientMessage::Ping).await;
    let pong = expect_kind(&client, "pong").await;
    assert_eq!(pong["type"], "pong");
}

// ---------------------------------------------------------------------------
// Connection accounting and shutdown timing (WP-PLT-01.T3): a server started through
// `ServerBuilder::start`, so `ServerHandle::open_connections()` is the figure under test.

mod counted {
    use std::future::Future;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use spot_core::{SiteId, SystemClock};
    use spot_http::{ServerBuilder, ServerConfig, ServerHandle, StaticSites};
    use spot_realtime::ws::{CancellationToken, Rejected, TokenVerifier, WsConfig};
    use spot_realtime::{AllowAll, BusConfig, Principal, RealtimeBus};

    use super::support::{self, Client, ClientMessage, expect_kind};

    /// Any `t-<n>` token is user `n`.
    struct NumberedUsers;

    impl TokenVerifier for NumberedUsers {
        fn verify<'a>(
            &'a self,
            _site: &'a SiteId,
            token: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Principal, Rejected>> + Send + 'a>> {
            Box::pin(async move {
                token
                    .strip_prefix("t-")
                    .and_then(|n| n.parse::<usize>().ok())
                    .map(|n| Principal::user(format!("0192f3c4-5b6a-7c8d-9e0f-a{n:011x}")))
                    .ok_or_else(|| Rejected::new("not a numbered token"))
            })
        }
    }

    pub struct Counted {
        pub handle: ServerHandle,
        pub addr: SocketAddr,
        pub shutdown: CancellationToken,
    }

    pub async fn start() -> Counted {
        let site = SiteId::new("acme.example").unwrap_or_else(|e| panic!("site: {e}"));
        let bus = RealtimeBus::new(
            BusConfig::default(),
            Arc::new(SystemClock::new()),
            Arc::new(AllowAll),
        );
        let shutdown = CancellationToken::new();
        let config = ServerConfig {
            bind: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)],
            ..ServerConfig::default()
        };
        let handle = ServerBuilder::new(
            config,
            StaticSites::new()
                .add("127.0.0.1", site.clone())
                .add_site_name(site),
        )
        .site_routes(spot_realtime::ws::router(
            bus,
            Arc::new(NumberedUsers),
            WsConfig {
                heartbeat_seconds: 25,
                shutdown: shutdown.clone(),
            },
        ))
        .start(std::future::pending())
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
        let addr = *handle
            .local_addrs()
            .first()
            .unwrap_or_else(|| panic!("no bound address"));
        Counted {
            handle,
            addr,
            shutdown,
        }
    }

    pub async fn connect_user(addr: SocketAddr, n: usize) -> Client {
        let client = support::connect(addr).await;
        client
            .send_message(&ClientMessage::Auth {
                token: format!("t-{n}"),
            })
            .await;
        expect_kind(&client, "ready").await;
        client
    }

    pub async fn wait_for_open(handle: &ServerHandle, wanted: usize, wait: Duration) -> usize {
        let started = Instant::now();
        loop {
            let open = handle.open_connections();
            if open == wanted || started.elapsed() > wait {
                return open;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// An open WebSocket is counted by `open_connections()` (it left the accept loop's task set at
/// the upgrade, but its session holds the connection lease), and the count drops when the
/// WebSocket closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connection_count_includes_an_open_websocket_and_drops_when_it_closes() {
    let server = counted::start().await;
    assert_eq!(server.handle.open_connections(), 0);
    let first = counted::connect_user(server.addr, 1).await;
    let second = counted::connect_user(server.addr, 2).await;
    assert_eq!(
        counted::wait_for_open(&server.handle, 2, Duration::from_secs(5)).await,
        2,
        "two authenticated WebSockets must be two connections"
    );
    // The client closes one socket: its session ends and the count follows.
    first.close(None).await;
    assert_eq!(
        counted::wait_for_open(&server.handle, 1, Duration::from_secs(5)).await,
        1
    );
    // The other is dropped without a close handshake.
    drop(second);
    assert_eq!(
        counted::wait_for_open(&server.handle, 0, Duration::from_secs(5)).await,
        0
    );
}

/// Shutting 200 well-behaved WebSockets down takes far less than one `CLOSE_LINGER` each: the
/// sockets close concurrently and the linger ends when the peer answers or hangs up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_of_two_hundred_well_behaved_websockets_takes_under_half_a_second() {
    let server = counted::start().await;
    let mut connecting = tokio::task::JoinSet::new();
    for n in 0..200 {
        let addr = server.addr;
        connecting.spawn(async move { counted::connect_user(addr, n).await });
    }
    let mut clients = Vec::with_capacity(200);
    while let Some(client) = connecting.join_next().await {
        clients.push(client.unwrap_or_else(|e| panic!("connect task: {e}")));
    }
    assert_eq!(
        counted::wait_for_open(&server.handle, 200, Duration::from_secs(10)).await,
        200
    );
    // Every client reads its close and hangs up, as a browser does.
    let mut closing = tokio::task::JoinSet::new();
    for client in clients {
        closing.spawn(async move { client.read_until_close().await });
    }
    let began = Instant::now();
    server.shutdown.cancel();
    while let Some(code) = closing.join_next().await {
        assert_eq!(code.unwrap_or_else(|e| panic!("close task: {e}")), 1001);
    }
    assert_eq!(
        counted::wait_for_open(&server.handle, 0, Duration::from_secs(5)).await,
        0
    );
    let elapsed = began.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "shutting down 200 well-behaved sockets took {elapsed:?}"
    );
}
