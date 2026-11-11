//! A small loopback TCP proxy, so [`crate::Node::partition_from`] can cut a node's sync traffic
//! mid-batch and [`crate::Node::heal`] can restore it.
//!
//! The proxy listens on an ephemeral loopback port and forwards every byte, in both directions,
//! to a fixed upstream address. Pausing stops both forwarding loops from reading further bytes
//! (bytes already written by either side sit unread until resumed), so a cut can land in the
//! middle of a write; resuming lets the same connection carry on. No business meaning is
//! attached to the bytes: the proxy is pure plumbing, proven by the tests below with a plain
//! echo upstream.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// How often a paused forwarding loop re-checks whether it may resume.
const PAUSE_POLL: Duration = Duration::from_millis(20);
/// How long a forwarding read waits before re-checking the pause flag even while it is not
/// paused, so a pause taken mid-read is honoured promptly instead of blocking on a read that
/// may never arrive.
const READ_POLL: Duration = Duration::from_millis(25);

/// A running loopback proxy in front of one upstream address.
pub struct Proxy {
    public_addr: SocketAddr,
    paused: Arc<AtomicBool>,
    shutdown: Option<oneshot::Sender<()>>,
    accept_task: Option<JoinHandle<()>>,
}

impl Proxy {
    /// Starts a proxy on an ephemeral loopback port, forwarding to `upstream`.
    ///
    /// # Panics
    /// When the loopback listener cannot be bound: a test environment that cannot open a
    /// loopback socket cannot run this harness at all.
    pub async fn spawn(upstream: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("cannot bind the loopback proxy: {e}"));
        let public_addr = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("cannot read the proxy's bound address: {e}"));
        let paused = Arc::new(AtomicBool::new(false));
        let (tx, rx) = oneshot::channel();
        let accept_task = tokio::spawn(accept_loop(listener, upstream, paused.clone(), rx));
        Proxy {
            public_addr,
            paused,
            shutdown: Some(tx),
            accept_task: Some(accept_task),
        }
    }

    /// The address a client dials instead of the upstream directly.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.public_addr
    }

    /// Stops forwarding in both directions, on every connection through this proxy, without
    /// closing anything: bytes already in flight sit unread until [`Proxy::resume`].
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Resumes forwarding.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    /// Whether the proxy is paused right now.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    upstream: SocketAddr,
    paused: Arc<AtomicBool>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((downstream, _)) = accepted else { continue; };
                tokio::spawn(serve_connection(downstream, upstream, paused.clone()));
            }
            _ = &mut shutdown => return,
        }
    }
}

async fn serve_connection(downstream: TcpStream, upstream: SocketAddr, paused: Arc<AtomicBool>) {
    let Ok(up) = TcpStream::connect(upstream).await else {
        return;
    };
    let (down_read, down_write) = downstream.into_split();
    let (up_read, up_write) = up.into_split();
    let forward = tokio::spawn(pump(down_read, up_write, paused.clone()));
    let backward = tokio::spawn(pump(up_read, down_write, paused));
    let _ = tokio::join!(forward, backward);
}

/// Copies bytes from `from` to `to` until either side closes, pausing the whole loop (not
/// consuming any bytes) while `paused` is set.
async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    paused: Arc<AtomicBool>,
) {
    let mut buffer = [0u8; 8192];
    loop {
        if paused.load(Ordering::SeqCst) {
            tokio::time::sleep(PAUSE_POLL).await;
            continue;
        }
        let read = tokio::time::timeout(READ_POLL, from.read(&mut buffer)).await;
        let n = match read {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return,
            Err(_) => continue, // the read timed out; re-check the pause flag and try again
        };
        if to.write_all(&buffer[..n]).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::net::TcpListener as TestListener;

    /// A plain echo server: whatever a client sends comes straight back.
    async fn spawn_echo() -> SocketAddr {
        let listener = TestListener::bind("127.0.0.1:0").await.expect("bind echo");
        let addr = listener.local_addr().expect("echo addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    loop {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if socket.write_all(&buffer[..n]).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn bytes_flow_through_an_unpaused_proxy() {
        let upstream = spawn_echo().await;
        let proxy = Proxy::spawn(upstream).await;
        let mut client = TcpStream::connect(proxy.addr()).await.expect("connect");
        client.write_all(b"hello").await.expect("write");
        let mut buffer = [0u8; 5];
        client.read_exact(&mut buffer).await.expect("read");
        assert_eq!(&buffer, b"hello");
    }

    #[tokio::test]
    async fn a_pause_blocks_bytes_both_ways_and_heal_restores_them() {
        let upstream = spawn_echo().await;
        let proxy = Proxy::spawn(upstream).await;
        let mut client = TcpStream::connect(proxy.addr()).await.expect("connect");

        // Before the pause: a normal round trip proves the connection and the echo work.
        client.write_all(b"before").await.expect("write before");
        let mut buffer = [0u8; 6];
        client.read_exact(&mut buffer).await.expect("read before");
        assert_eq!(&buffer, b"before");

        // Mid-connection cut: a batch written while paused never arrives.
        proxy.pause();
        // Let any read already in flight finish its poll cycle and observe the pause flag
        // before we write new bytes, so this assertion is not racing the proxy's own loop.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(proxy.is_paused());
        client.write_all(b"during").await.expect("write during");
        let mut probe = [0u8; 1];
        let blocked =
            tokio::time::timeout(Duration::from_millis(300), client.read(&mut probe)).await;
        assert!(
            blocked.is_err(),
            "bytes crossed a paused proxy: {blocked:?}"
        );

        // Heal: the same bytes resume flowing without a new connection.
        proxy.resume();
        assert!(!proxy.is_paused());
        let mut buffer = [0u8; 6];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut buffer))
            .await
            .expect("read after heal timed out")
            .expect("read after heal");
        assert_eq!(&buffer, b"during");
    }
}
