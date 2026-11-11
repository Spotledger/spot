//! Client-side helpers shared by the loopback and socket suites.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use spot_link_transport::{ClientConfig, LinkCarriers, LinkClient, PeerTransport, Resilience};
use spot_sync_http::{
    BoxFuture, BreakerConfig, Error as SyncError, HttpRequest, HttpResponse, NoJitter, NoopSleeper,
    RetryPolicy, Transport,
};
use tokio::sync::Semaphore;

use super::Node;

/// Quick settings: tiny backoff, a short long-poll, no idle pause worth noticing.
pub fn config() -> ClientConfig {
    ClientConfig {
        deadline: Duration::from_secs(10),
        retry: RetryPolicy::new(3, Duration::from_millis(1), 2, Duration::from_millis(20)).unwrap(),
        breaker: BreakerConfig::new(20, Duration::from_secs(60), 1).unwrap(),
        push_batch: 16,
        pull_batch: 16,
        pull_wait: Duration::from_millis(100),
        idle_wait: Duration::from_millis(20),
        unreachable_after_ms: 0,
    }
}

/// A client of `local` talking to `peer` over `transport`.
pub fn client(
    local: &Node,
    peer: &Node,
    transport: Arc<dyn Transport>,
    carriers: LinkCarriers,
    config: ClientConfig,
) -> LinkClient {
    LinkClient::new(
        local.host.clone(),
        local.link,
        PeerTransport::new(transport, peer.party.clone()),
        carriers,
        config,
        Resilience {
            jitter: Box::new(NoJitter),
            sleeper: Arc::new(NoopSleeper),
            bulkhead: Arc::new(Semaphore::new(4)),
        },
    )
    .unwrap()
}

/// A transport that can swallow replies (after the server did its work), or fail outright.
pub struct Flaky {
    inner: Arc<dyn Transport>,
    /// How many of the next replies to swallow after forwarding the request.
    pub swallow: AtomicUsize,
    /// Fail every request without forwarding it.
    pub down: AtomicBool,
    /// Fail every acknowledgement request without forwarding it.
    pub fail_acks: AtomicBool,
    /// How many requests reached the server.
    pub forwarded: AtomicUsize,
}

impl Flaky {
    pub fn new(inner: Arc<dyn Transport>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            swallow: AtomicUsize::new(0),
            down: AtomicBool::new(false),
            fail_acks: AtomicBool::new(false),
            forwarded: AtomicUsize::new(0),
        })
    }
}

impl Transport for Flaky {
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse, SyncError>> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { Err(SyncError::unavailable("the peer is down")) });
        }
        if self.fail_acks.load(Ordering::SeqCst) && request.path.ends_with("/acks") {
            return Box::pin(async { Err(SyncError::unavailable("the ack was lost")) });
        }
        let inner = Arc::clone(&self.inner);
        let swallow = self.swallow.load(Ordering::SeqCst) > 0;
        if swallow {
            self.swallow.fetch_sub(1, Ordering::SeqCst);
        }
        self.forwarded.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let response = inner.request(request).await?;
            if swallow {
                return Err(SyncError::unavailable("the reply was lost"));
            }
            Ok(response)
        })
    }
}

/// The error code of a failed response body.
pub fn code_of(response: &HttpResponse) -> String {
    let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    value["error"]["code"].as_str().unwrap().to_owned()
}
