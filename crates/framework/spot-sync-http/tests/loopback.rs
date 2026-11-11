//! The binding over the loopback transport: the real handlers, the real middleware and the real
//! client, with no socket. This is the T1 acceptance property carried through the wire.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(missing_docs)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use support::*;

use spot_core::ManualClock;
use spot_sync_http::error::{
    BATCH_TOO_LARGE, CIRCUIT_OPEN, DEADLINE_EXCEEDED, IDENTITY_UNKNOWN, PARTITION_VIOLATION,
    UNAVAILABLE,
};
use spot_sync_http::server::{PULL_PATH, pull_query};
use spot_sync_http::{
    BoxFuture, BreakerConfig, ClientConfig, Error, HttpRequest, HttpResponse, LoopbackTransport,
    NoJitter, Sleeper, Transport, wire,
};
use spot_sync_protocol::{NodeId, Offer, RetryPolicy, SourceId};

/// A pull converges over the real handlers: 200 records, applied exactly once, in order.
#[tokio::test]
async fn a_pull_converges_over_the_real_handlers() {
    let source = SourceId::new("head-office#hub").unwrap();
    let stream = Arc::new(spot_sync_http::MemoryHub::new());
    stream.append(&source, records(&source, 1, 200));
    let cert = certificate(1);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        stream,
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let client = client(&hub, &node, cert);
    let reply = client.hello().await.unwrap();
    assert_eq!(reply.hub.node, hub.node);

    let mut receiver = receiver();
    let applied = client
        .pull_until_idle(&mut receiver, &source, 50)
        .await
        .unwrap();
    assert_eq!(applied, 200);
    assert_eq!(receiver.sink().len(), 200);
    let seqs: Vec<u64> = receiver.sink().order().iter().map(|a| a.seq).collect();
    assert_eq!(seqs, (1..=200).collect::<Vec<_>>());
    assert_eq!(
        receiver
            .stored_checkpoint(&source)
            .unwrap()
            .last_applied_seq,
        200
    );
}

/// A push converges: 100 records, applied exactly once at the hub.
#[tokio::test]
async fn a_push_converges_over_the_real_handlers() {
    let source = SourceId::new("edge-khi#branch_01").unwrap();
    let cert = certificate(2);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        Arc::new(spot_sync_http::MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let mut client = client(&hub, &node, cert);
    let offer = Offer::new(source.clone(), records(&source, 1, 100));
    let acked = client.push_until_idle(&offer, 50).await.unwrap();
    assert_eq!(acked, 100);
    assert_eq!(hub.sink.with(|r| r.sink().len()), 100);
    assert_eq!(hub.sink.with(|r| r.sink().order().len()), 100);
}

/// A batch whose source partition the node does not own is refused before any record is applied.
#[tokio::test]
async fn an_unowned_partition_is_refused() {
    let source = SourceId::new("edge-khi#branch_02").unwrap();
    let cert = certificate(3);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        Arc::new(spot_sync_http::MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let mut client = client(&hub, &node, cert);
    let offer = Offer::new(source.clone(), records(&source, 1, 3));
    let error = client.push_until_idle(&offer, 5).await.unwrap_err();
    assert_eq!(error.code, PARTITION_VIOLATION);
    assert_eq!(hub.sink.with(|r| r.sink().len()), 0, "nothing was applied");
}

/// A node cannot pull another site's or another node's stream.
#[tokio::test]
async fn a_foreign_stream_cannot_be_pulled() {
    let foreign = SourceId::new("other-tenant#branch_01").unwrap();
    let sibling = SourceId::new("edge-khi#branch_02").unwrap();
    let stream = Arc::new(spot_sync_http::MemoryHub::new());
    stream.append(&foreign, records(&foreign, 1, 3));
    stream.append(&sibling, records(&sibling, 1, 3));
    let cert = certificate(5);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        stream,
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );
    let client = client(&hub, &node, cert);
    for source in [&foreign, &sibling] {
        let mut rx = receiver();
        let error = client
            .pull_until_idle(&mut rx, source, 5)
            .await
            .unwrap_err();
        assert_eq!(error.code, PARTITION_VIOLATION);
        assert_eq!(rx.sink().len(), 0);
    }
}

/// A certificate the directory does not know is refused.
#[tokio::test]
async fn an_unenrolled_certificate_is_refused() {
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let enrolled = certificate(4);
    let presented = certificate(40);
    let hub = hub(
        Arc::new(directory(&enrolled, &node, &["branch_01"])),
        Arc::new(spot_sync_http::MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let client = client(&hub, &node, presented);
    let error = client.hello().await.unwrap_err();
    assert_eq!(error.code, IDENTITY_UNKNOWN);
}

/// A batch above the hub's record limit is refused with `sync_http.batch_too_large`.
#[tokio::test]
async fn an_oversize_batch_is_refused() {
    let source = SourceId::new("edge-khi#branch_01").unwrap();
    let cert = certificate(5);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let mut small = settings();
    small.max_batch_records = 2;
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        Arc::new(spot_sync_http::MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        small,
    );

    let mut client = client_with(&hub, &node, cert, client_config(8));
    let offer = Offer::new(source.clone(), records(&source, 1, 5));
    let error = client.push_until_idle(&offer, 5).await.unwrap_err();
    assert_eq!(error.code, BATCH_TOO_LARGE);
}

/// An idle long-poll answers with heartbeats and no batch, then returns a batch once one arrives.
#[tokio::test]
async fn a_long_poll_heartbeats_then_serves() {
    let source = SourceId::new("head-office#hub").unwrap();
    let stream = Arc::new(spot_sync_http::MemoryHub::new());
    let cert = certificate(6);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        stream.clone(),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let transport = LoopbackTransport::new(hub.router.clone(), cert);
    let request = HttpRequest::get(PULL_PATH).with_query(pull_query(&source, 1, 8));

    let idle = transport.request(request.clone()).await.unwrap();
    assert_eq!(idle.status, 200);
    assert!(!wire::contains_batch(&idle.body), "nothing was available");
    assert!(
        idle.text().unwrap().contains("\"heartbeat\":true"),
        "the idle poll heartbeated: {}",
        idle.text().unwrap()
    );

    stream.append(&source, records(&source, 1, 3));
    let served = transport.request(request).await.unwrap();
    assert!(wire::contains_batch(&served.body));
    let batch = wire::decode_batch(&served.body).unwrap();
    assert_eq!((batch.from_seq, batch.to_seq), (1, 3));
}

/// A push delivered twice is applied once: the hub's durable apply is idempotent by key.
#[tokio::test]
async fn a_duplicate_push_is_applied_once() {
    let source = SourceId::new("edge-khi#branch_01").unwrap();
    let cert = certificate(7);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        Arc::new(spot_sync_http::MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let inner: Arc<dyn Transport> =
        Arc::new(LoopbackTransport::new(hub.router.clone(), cert.clone()));
    let transport: Arc<dyn Transport> = Arc::new(DuplicatingTransport { inner });
    let mut client = spot_sync_http::EdgeClient::new(
        transport,
        identity(&node),
        client_config(64),
        Arc::new(spot_core::SystemClock::new()),
        Box::new(NoJitter),
        Arc::new(spot_sync_http::NoopSleeper),
        "hub:head-office",
    )
    .unwrap();

    let offer = Offer::new(source.clone(), records(&source, 1, 5));
    let acked = client.push_until_idle(&offer, 50).await.unwrap();
    assert_eq!(acked, 5);
    assert_eq!(hub.sink.with(|r| r.sink().len()), 5);
    assert_eq!(
        hub.sink.with(|r| r.sink().order().len()),
        5,
        "no record twice"
    );
}

/// A transient transport failure is retried within the session's budget.
#[tokio::test]
async fn a_transient_failure_is_retried() {
    let cert = certificate(8);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        Arc::new(spot_sync_http::MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let inner: Arc<dyn Transport> = Arc::new(LoopbackTransport::new(hub.router.clone(), cert));
    let transport: Arc<dyn Transport> = Arc::new(FlakyTransport {
        inner,
        failures_left: AtomicUsize::new(2),
    });
    let client = spot_sync_http::EdgeClient::new(
        transport,
        identity(&node),
        client_config(64),
        Arc::new(spot_core::SystemClock::new()),
        Box::new(NoJitter),
        Arc::new(spot_sync_http::NoopSleeper),
        "hub:head-office",
    )
    .unwrap();

    assert!(client.hello().await.is_ok(), "two failures, then success");
}

/// Repeated failures open the breaker, which then refuses without dialling.
#[tokio::test]
async fn the_breaker_opens_after_repeated_failures() {
    let cert = certificate(9);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        Arc::new(spot_sync_http::MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let transport: Arc<dyn Transport> = Arc::new(FailingTransport);
    let config = ClientConfig::new(
        Duration::from_secs(30),
        RetryPolicy::new(3, Duration::from_millis(1), 2, Duration::from_millis(10)).unwrap(),
        BreakerConfig::new(5, Duration::from_secs(30), 1).unwrap(),
        64,
    )
    .unwrap();
    let client = spot_sync_http::EdgeClient::new(
        transport,
        identity(&node),
        config,
        Arc::new(spot_core::SystemClock::new()),
        Box::new(NoJitter),
        Arc::new(spot_sync_http::NoopSleeper),
        format!("hub:{}", hub.node),
    )
    .unwrap();

    let first = client.hello().await.unwrap_err();
    assert_eq!(first.code, UNAVAILABLE, "three attempts, all failed");
    let second = client.hello().await.unwrap_err();
    assert_eq!(second.code, CIRCUIT_OPEN, "the breaker opened");
}

/// A retry that would sleep past the deadline fails now, with `sync_http.deadline_exceeded`.
#[tokio::test]
async fn a_retry_past_the_deadline_fails_now() {
    let cert = certificate(10);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let _hub = hub(
        Arc::new(directory(&cert, &node, &["branch_01"])),
        Arc::new(spot_sync_http::MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
        settings(),
    );

    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let config = ClientConfig::new(
        Duration::from_millis(100),
        RetryPolicy::new(3, Duration::from_millis(100), 2, Duration::from_secs(5)).unwrap(),
        BreakerConfig::default(),
        64,
    )
    .unwrap();
    let transport: Arc<dyn Transport> = Arc::new(FailingTransport);
    let client = spot_sync_http::EdgeClient::new(
        transport,
        identity(&node),
        config,
        clock.clone(),
        Box::new(NoJitter),
        Arc::new(AdvancingSleeper {
            clock: clock.clone(),
        }),
        "hub:head-office",
    )
    .unwrap();

    let error = client.hello().await.unwrap_err();
    assert_eq!(error.code, DEADLINE_EXCEEDED);
}

/// The loopback analogue of the T1 simulator: over a lossy transport, a pull and a push still
/// converge exactly once, for several seeds.
#[tokio::test]
async fn a_lossy_loopback_still_converges() {
    for seed in 0..24_u64 {
        let pull_source = SourceId::new("head-office#hub").unwrap();
        let push_source = SourceId::new("edge-khi#branch_01").unwrap();
        let stream = Arc::new(spot_sync_http::MemoryHub::new());
        stream.append(&pull_source, records(&pull_source, 1, 120));
        let cert = certificate(11);
        let node = NodeId::new("edge-khi#branch_01").unwrap();
        let hub = hub(
            Arc::new(directory(&cert, &node, &["branch_01"])),
            stream,
            Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
            settings(),
        );

        let inner: Arc<dyn Transport> = Arc::new(LoopbackTransport::new(hub.router.clone(), cert));
        let transport: Arc<dyn Transport> = Arc::new(FaultyTransport {
            inner,
            state: std::sync::Mutex::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
        });
        // A breaker that never opens during the run: this test is about convergence, and the
        // breaker has its own test.
        let config = ClientConfig::new(
            Duration::from_secs(30),
            RetryPolicy::new(4, Duration::from_millis(1), 2, Duration::from_millis(10)).unwrap(),
            BreakerConfig::new(10_000, Duration::ZERO, 1).unwrap(),
            32,
        )
        .unwrap();
        let mut client = spot_sync_http::EdgeClient::new(
            transport,
            identity(&node),
            config,
            Arc::new(spot_core::SystemClock::new()),
            Box::new(NoJitter),
            Arc::new(spot_sync_http::NoopSleeper),
            "hub:head-office",
        )
        .unwrap();

        let mut receiver = receiver();
        let mut applied = 0;
        for _ in 0..200 {
            match client
                .pull_until_idle(&mut receiver, &pull_source, 20)
                .await
            {
                Ok(through) => applied = applied.max(through),
                Err(error) => assert_eq!(error.code, UNAVAILABLE, "seed {seed}: {}", error.message),
            }
            if applied == 120 {
                break;
            }
        }
        assert_eq!(applied, 120, "seed {seed} did not converge");
        assert_eq!(receiver.sink().len(), 120);

        let offer = Offer::new(push_source.clone(), records(&push_source, 1, 80));
        let mut acked = 0;
        for _ in 0..200 {
            match client.push_until_idle(&offer, 20).await {
                Ok(through) => acked = acked.max(through),
                Err(error) => assert_eq!(error.code, UNAVAILABLE, "seed {seed}: {}", error.message),
            }
            if acked == 80 {
                break;
            }
        }
        assert_eq!(acked, 80, "seed {seed} did not push");
        assert_eq!(hub.sink.with(|r| r.sink().len()), 80);
        assert_eq!(
            hub.sink.with(|r| r.sink().order().len()),
            80,
            "seed {seed} applied a record twice"
        );
    }
}

/// Sends every request twice and returns the first answer.
struct DuplicatingTransport {
    inner: Arc<dyn Transport>,
}

impl Transport for DuplicatingTransport {
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse, Error>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let first = inner.request(request.clone()).await?;
            let _ = inner.request(request).await?;
            Ok(first)
        })
    }
}

/// Fails the first `failures_left` calls, then delegates.
struct FlakyTransport {
    inner: Arc<dyn Transport>,
    failures_left: AtomicUsize,
}

impl Transport for FlakyTransport {
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse, Error>> {
        let inner = Arc::clone(&self.inner);
        let fail = self
            .failures_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        Box::pin(async move {
            if fail {
                return Err(Error::unavailable("injected transient failure"));
            }
            inner.request(request).await
        })
    }
}

/// Always fails.
struct FailingTransport;

impl Transport for FailingTransport {
    fn request(&self, _request: HttpRequest) -> BoxFuture<Result<HttpResponse, Error>> {
        Box::pin(async { Err(Error::unavailable("the hub is down")) })
    }
}

/// A seeded, lossy transport: it drops a request, or loses a response after the hub processed it.
struct FaultyTransport {
    inner: Arc<dyn Transport>,
    state: std::sync::Mutex<u64>,
}

impl FaultyTransport {
    /// The next 64 bits.
    fn draw(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl Transport for FaultyTransport {
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse, Error>> {
        let roll = self.draw() % 100;
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            if roll < 15 {
                // The request never reached the hub.
                return Err(Error::unavailable("injected drop"));
            }
            let response = inner.request(request.clone()).await?;
            if roll < 30 {
                // The hub processed it, but the response was lost; the retry re-sends.
                return Err(Error::unavailable("injected lost response"));
            }
            Ok(response)
        })
    }
}

/// A sleeper that advances a manual clock, so the deadline is exercised deterministically.
struct AdvancingSleeper {
    clock: Arc<ManualClock>,
}

impl Sleeper for AdvancingSleeper {
    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        self.clock.advance(duration);
        Box::pin(std::future::ready(()))
    }
}
