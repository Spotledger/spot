//! Connection accounting and shutdown timing (WP-PLT-01.T3): HTTP/1.1 and HTTP/2 connection
//! counts return to zero after shutdown, and an HTTP/2 client that never answers hyper's
//! graceful-shutdown `PING` does not hold shutdown past the grace (with no request in flight,
//! not at all). The HTTP/2 client is a raw-socket frame writer: no HTTP client dependency, and
//! it can refuse to answer a `PING`, which a real client library will not.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use axum::routing::get;
use spot_core::SiteId;
use spot_http::{ServerBuilder, ServerConfig, ServerHandle, StaticSites};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const HOST: &str = "acme.spotledger.app";

/// Test-only unwrapping that names the failure (`unwrap_used` and `expect_used` are denied).
trait Must<T> {
    fn must(self) -> T;
}

impl<T, E: std::fmt::Debug> Must<T> for Result<T, E> {
    fn must(self) -> T {
        self.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
    }
}

impl<T> Must<T> for Option<T> {
    fn must(self) -> T {
        self.unwrap_or_else(|| panic!("unexpected None"))
    }
}

async fn quick() -> &'static str {
    "quick"
}

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_millis(600)).await;
    "done"
}

async fn hang() -> &'static str {
    tokio::time::sleep(Duration::from_secs(120)).await;
    "never"
}

async fn start(grace: Duration) -> (ServerHandle, SocketAddr, oneshot::Sender<()>) {
    let site = SiteId::new("acme").must();
    let config = ServerConfig {
        bind: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)],
        shutdown_grace: grace,
        ..ServerConfig::default()
    };
    let routes = axum::Router::new()
        .route("/quick", get(quick))
        .route("/slow", get(slow))
        .route("/hang", get(hang));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let handle = ServerBuilder::new(
        config,
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(routes)
    .start(async move {
        let _ = stop_rx.await;
    })
    .await
    .must();
    let addr = *handle.local_addrs().first().must();
    (handle, addr, stop_tx)
}

async fn wait_for_open(handle: &ServerHandle, wanted: usize, wait: Duration) -> usize {
    let started = Instant::now();
    loop {
        let open = handle.open_connections();
        if open == wanted || started.elapsed() > wait {
            return open;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// An HTTP/1.1 keep-alive connection that has completed one request and stays idle.
async fn h1_idle_connection(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.must();
    let request = format!("GET /quick HTTP/1.1\r\nhost: {HOST}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.must();
    let mut seen = Vec::new();
    let mut chunk = [0u8; 1024];
    while !seen.ends_with(b"quick") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .must()
            .must();
        assert!(n > 0, "the server closed before answering");
        seen.extend_from_slice(chunk.get(..n).must());
    }
    stream
}

/// One HTTP/2 frame as read off the wire.
struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

const FRAME_DATA: u8 = 0;
const FRAME_SETTINGS: u8 = 4;
const FRAME_PING: u8 = 6;
const FRAME_GOAWAY: u8 = 7;
const FLAG_END_STREAM: u8 = 1;

/// Reads one frame; `None` when the server closed the connection (EOF or reset).
async fn read_frame(stream: &mut TcpStream) -> Option<Frame> {
    let mut head = [0u8; 9];
    stream.read_exact(&mut head).await.ok()?;
    let length = (usize::from(head[0]) << 16) | (usize::from(head[1]) << 8) | usize::from(head[2]);
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await.ok()?;
    Some(Frame {
        kind: head[3],
        flags: head[4],
        stream: u32::from_be_bytes([head[5] & 0x7f, head[6], head[7], head[8]]),
        payload,
    })
}

/// Opens an HTTP/2 connection (prior knowledge) and returns it after the server's SETTINGS.
async fn h2_connect(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.must();
    let mut hello = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    // An empty SETTINGS frame.
    hello.extend_from_slice(&[0, 0, 0, FRAME_SETTINGS, 0, 0, 0, 0, 0]);
    stream.write_all(&hello).await.must();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream))
            .await
            .must()
            .must();
        if frame.kind == FRAME_SETTINGS && frame.flags & 1 == 0 {
            return stream;
        }
    }
}

/// Sends `GET <path>` on stream 1 (HPACK literals without indexing, so no encoder is needed).
async fn h2_get(stream: &mut TcpStream, path: &str) {
    let mut block = vec![0x82, 0x86, 0x04, u8::try_from(path.len()).must()];
    block.extend_from_slice(path.as_bytes());
    block.extend_from_slice(&[0x01, u8::try_from(HOST.len()).must()]);
    block.extend_from_slice(HOST.as_bytes());
    // The site resolver reads the `host` header, which HTTP/2 clients also send.
    block.extend_from_slice(&[0x00, 4]);
    block.extend_from_slice(b"host");
    block.push(u8::try_from(HOST.len()).must());
    block.extend_from_slice(HOST.as_bytes());
    let length = block.len();
    // HEADERS, END_STREAM | END_HEADERS, stream 1.
    let mut frame = vec![0, 0, u8::try_from(length).must(), 1, 0x05, 0, 0, 0, 1];
    frame.extend_from_slice(&block);
    stream.write_all(&frame).await.must();
}

/// What a client that never answers a `PING` saw before the server closed.
struct Outcome {
    goaway: bool,
    ping: bool,
    body: Vec<u8>,
    closed_after: Duration,
}

/// Reads frames until the server closes, never answering a `PING`; gives up after `limit`.
async fn read_until_closed(stream: &mut TcpStream, began: Instant, limit: Duration) -> Outcome {
    let mut outcome = Outcome {
        goaway: false,
        ping: false,
        body: Vec::new(),
        closed_after: Duration::ZERO,
    };
    loop {
        let remaining = limit.saturating_sub(began.elapsed());
        match tokio::time::timeout(remaining, read_frame(stream)).await {
            Err(_) => panic!("the server held the connection open past {limit:?}"),
            Ok(None) => {
                outcome.closed_after = began.elapsed();
                return outcome;
            }
            Ok(Some(frame)) => match frame.kind {
                FRAME_GOAWAY => outcome.goaway = true,
                FRAME_PING => outcome.ping = true,
                FRAME_DATA if frame.stream == 1 => {
                    outcome.body.extend_from_slice(&frame.payload);
                    let _ = frame.flags & FLAG_END_STREAM;
                }
                _ => {}
            },
        }
    }
}

#[tokio::test]
async fn http1_connections_are_counted_and_return_to_zero_after_shutdown() {
    let (handle, addr, stop) = start(Duration::from_secs(30)).await;
    let first = h1_idle_connection(addr).await;
    let second = h1_idle_connection(addr).await;
    assert_eq!(wait_for_open(&handle, 2, Duration::from_secs(5)).await, 2);
    let began = Instant::now();
    let _ = stop.send(());
    assert_eq!(wait_for_open(&handle, 0, Duration::from_secs(5)).await, 0);
    tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .must()
        .must();
    assert!(began.elapsed() < Duration::from_secs(5));
    drop((first, second));
}

#[tokio::test]
async fn http2_connections_are_counted_and_return_to_zero_after_shutdown() {
    let (handle, addr, stop) = start(Duration::from_secs(30)).await;
    let mut clients = vec![h2_connect(addr).await, h2_connect(addr).await];
    assert_eq!(wait_for_open(&handle, 2, Duration::from_secs(5)).await, 2);
    let began = Instant::now();
    let _ = stop.send(());
    for client in &mut clients {
        // The client answers nothing, yet the connection must end.
        let outcome = read_until_closed(client, began, Duration::from_secs(5)).await;
        assert!(outcome.goaway, "the server must say GOAWAY before closing");
    }
    assert_eq!(wait_for_open(&handle, 0, Duration::from_secs(5)).await, 0);
    tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .must()
        .must();
}

/// With no request in flight, a client that never answers the shutdown `PING` does not hold
/// the connection at all: it is closed right after the `GOAWAY`, not at the 30 s grace.
#[tokio::test]
async fn http2_idle_client_that_ignores_the_ping_does_not_hold_shutdown() {
    let (handle, addr, stop) = start(Duration::from_secs(30)).await;
    let mut client = h2_connect(addr).await;
    assert_eq!(wait_for_open(&handle, 1, Duration::from_secs(5)).await, 1);
    let began = Instant::now();
    let _ = stop.send(());
    let outcome = read_until_closed(&mut client, began, Duration::from_secs(3)).await;
    assert!(outcome.goaway, "GOAWAY must precede the close");
    assert!(
        outcome.ping,
        "hyper pings after the first GOAWAY; the client ignores it"
    );
    assert!(
        outcome.closed_after < Duration::from_secs(2),
        "closed after {:?}",
        outcome.closed_after
    );
    assert_eq!(wait_for_open(&handle, 0, Duration::from_secs(2)).await, 0);
    tokio::time::timeout(Duration::from_secs(2), handle.join())
        .await
        .must()
        .must();
}

/// A request in flight is finished (its response arrives), then the connection ends without
/// waiting for the ignored `PING` or for the grace.
#[tokio::test]
async fn http2_in_flight_request_finishes_then_the_connection_ends() {
    let (handle, addr, stop) = start(Duration::from_secs(30)).await;
    let mut client = h2_connect(addr).await;
    h2_get(&mut client, "/slow").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let began = Instant::now();
    let _ = stop.send(());
    let outcome = read_until_closed(&mut client, began, Duration::from_secs(5)).await;
    assert_eq!(
        outcome.body, b"done",
        "the in-flight request must be answered"
    );
    assert!(
        outcome.closed_after < Duration::from_secs(3),
        "closed after {:?}",
        outcome.closed_after
    );
    assert_eq!(wait_for_open(&handle, 0, Duration::from_secs(2)).await, 0);
    tokio::time::timeout(Duration::from_secs(2), handle.join())
        .await
        .must()
        .must();
}

/// A request that outlives the grace is cut at the grace, with the client still ignoring the
/// `PING`: shutdown takes about the grace, not longer, and the count returns to zero.
#[tokio::test]
async fn http2_request_outliving_the_grace_is_cut_at_the_grace() {
    let grace = Duration::from_secs(1);
    let (handle, addr, stop) = start(grace).await;
    let mut client = h2_connect(addr).await;
    h2_get(&mut client, "/hang").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let began = Instant::now();
    let _ = stop.send(());
    let outcome = read_until_closed(&mut client, began, Duration::from_secs(6)).await;
    assert!(
        outcome.closed_after >= grace.saturating_sub(Duration::from_millis(100)),
        "closed after {:?}, before the grace",
        outcome.closed_after
    );
    assert!(outcome.closed_after < Duration::from_secs(4));
    assert_eq!(wait_for_open(&handle, 0, Duration::from_secs(2)).await, 0);
    tokio::time::timeout(Duration::from_secs(2), handle.join())
        .await
        .must()
        .must();
}
