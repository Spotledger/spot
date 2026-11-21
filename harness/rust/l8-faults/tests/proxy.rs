//! The proxy's fault semantics, against a trivial echo server (no PostgreSQL needed).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use spot_l8_faults::faults::{Direction, Fault, MessageTrigger, ScheduledFault, Trigger};
use spot_l8_faults::proxy::{FaultProxy, ProxyConfig};
use spot_l8_faults::wire::PgMessage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// An echo server that copies every byte back.
async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (mut r, mut w) = stream.into_split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    addr
}

async fn proxy() -> FaultProxy {
    let upstream = echo_server().await;
    FaultProxy::start(ProxyConfig::new(upstream)).await.unwrap()
}

async fn proxy_to(upstream: SocketAddr) -> FaultProxy {
    FaultProxy::start(ProxyConfig::new(upstream)).await.unwrap()
}

/// An echo server that reports the bytes each connection received.
async fn recording_echo() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                let mut received = Vec::new();
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            received.extend_from_slice(&buf[..n]);
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = tx.send(received);
            });
        }
    });
    (addr, rx)
}

/// Reads until EOF or a deadline, returning the bytes seen.
async fn read_to_end(client: &mut TcpStream, budget: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, client.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            Ok(Err(_)) => break,
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pass_relays_both_ways() {
    let proxy = proxy().await;
    proxy.schedule(ScheduledFault::new(
        Direction::ClientToServer,
        Trigger::Immediate,
        Fault::Pass,
    ));
    let mut client = TcpStream::connect(proxy.addr()).await.unwrap();
    client.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 16];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_after_bytes_relays_exactly_that_many() {
    let (upstream, mut received) = recording_echo().await;
    let proxy = proxy_to(upstream).await;
    proxy.schedule(ScheduledFault::drop_after_bytes(
        Direction::ClientToServer,
        4,
    ));
    let mut client = TcpStream::connect(proxy.addr()).await.unwrap();
    client.write_all(b"0123456789").await.unwrap();
    let _ = read_to_end(&mut client, Duration::from_secs(2)).await;
    let got = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .expect("the server reports what it received")
        .expect("a report");
    assert_eq!(got, b"0123", "exactly four bytes must reach the server");
    assert_eq!(proxy.events().len(), 1);
    assert_eq!(proxy.events()[0].fault, Fault::Drop);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delay_adds_latency() {
    let proxy = proxy().await;
    proxy.schedule(ScheduledFault::new(
        Direction::ServerToClient,
        Trigger::Immediate,
        Fault::delay(300),
    ));
    let mut client = TcpStream::connect(proxy.addr()).await.unwrap();
    let started = Instant::now();
    client.write_all(b"x").await.unwrap();
    let mut buf = [0u8; 4];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(n, 1);
    assert!(
        started.elapsed() >= Duration::from_millis(280),
        "the delay must be applied: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_now_closes_the_connection() {
    let proxy = proxy().await;
    proxy.schedule(ScheduledFault::new(
        Direction::ClientToServer,
        Trigger::Immediate,
        Fault::Reset,
    ));
    let mut client = TcpStream::connect(proxy.addr()).await.unwrap();
    let _ = client.write_all(b"gone").await;
    let seen = read_to_end(&mut client, Duration::from_secs(2)).await;
    assert!(seen.is_empty(), "nothing may be echoed after a reset");
    assert_eq!(proxy.events()[0].fault, Fault::Reset);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blackhole_swallows_then_heals() {
    let proxy = proxy().await;
    proxy.schedule(ScheduledFault::new(
        Direction::ClientToServer,
        Trigger::Immediate,
        Fault::blackhole(300),
    ));
    let mut client = TcpStream::connect(proxy.addr()).await.unwrap();
    client.write_all(b"swallowed").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    client.write_all(b"kept").await.unwrap();
    let mut buf = [0u8; 32];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(
        &buf[..n],
        b"kept",
        "only traffic after the window is relayed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_when_message_cuts_on_the_named_message() {
    let proxy = proxy().await;
    proxy.schedule(ScheduledFault::reset_when_message(
        Direction::ClientToServer,
        PgMessage::Sync,
        true,
    ));
    let mut client = TcpStream::connect(proxy.addr()).await.unwrap();
    // A startup packet (untyped) then a Sync frame.
    let mut stream = vec![0u8, 0, 0, 8, 0, 3, 0, 0];
    stream.extend_from_slice(&[b'S', 0, 0, 0, 4]);
    client.write_all(&stream).await.unwrap();
    let _ = read_to_end(&mut client, Duration::from_secs(2)).await;
    let events = proxy.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fault, Fault::Reset);
    assert_eq!(events[0].detail, "message=sync");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_trigger_skips_occurrences() {
    let proxy = proxy().await;
    // Cut on the second `Query` (the commit after BEGIN), relaying it first.
    proxy.schedule(ScheduledFault::new(
        Direction::ClientToServer,
        Trigger::OnMessage(MessageTrigger::every(PgMessage::Query, true, 1)),
        Fault::Reset,
    ));
    let mut client = TcpStream::connect(proxy.addr()).await.unwrap();
    let mut stream = vec![0u8, 0, 0, 8, 0, 3, 0, 0];
    stream.extend_from_slice(&[b'Q', 0, 0, 0, 5, b'B']);
    stream.extend_from_slice(&[b'Q', 0, 0, 0, 5, b'C']);
    client.write_all(&stream).await.unwrap();
    let _ = read_to_end(&mut client, Duration::from_secs(2)).await;
    assert_eq!(proxy.events().len(), 1);
    assert_eq!(proxy.events()[0].detail, "message=query");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_heals_a_blackhole() {
    let proxy = proxy().await;
    proxy.schedule(ScheduledFault::new(
        Direction::ClientToServer,
        Trigger::Immediate,
        Fault::blackhole(10_000),
    ));
    let mut client = TcpStream::connect(proxy.addr()).await.unwrap();
    client.write_all(b"a").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    proxy.clear();
    client.write_all(b"b").await.unwrap();
    let mut buf = [0u8; 8];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"b");
}
