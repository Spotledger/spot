//! An outage shorter than a probe interval is seen (WP-PLT-19.T8): the plugin counts failed
//! operations, not only the probe's verdict. No server needed: the coordinator is the in-process
//! one on a manual clock, behind the plugin's own `ObservedCoordinator` and a switch that makes
//! the three watched calls fail as a lost connection would.
//!
//! This file states the new expectation. The earlier pin of "today's behaviour" in
//! `cluster_redis_fencing.rs` (WP-PLT-19.T7: a sub-probe outage is invisible) is now wrong and
//! has to be replaced there (see the task report).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spot_cli::services::cluster_redis::{ObservedCoordinator, Settings, Status, validate};
use spot_cluster_redis::coordinator::UNAVAILABLE;
use spot_coordinator::error::LEASE_HELD;
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, FencingToken, InMemoryPresence, InProcessCoordinator, Lease,
    NodeId, RateLimit, Result as CoordResult,
};
use spot_core::{Clock, ManualClock};
use time::OffsetDateTime;
use tokio::sync::Notify;

const LEASE: &str = "job:acme";

/// Delegates to a coordinator, except that while `down` the three calls the plugin watches fail
/// as a lost connection would.
struct Cuttable {
    inner: Arc<dyn Coordinator>,
    down: Arc<AtomicBool>,
}

fn unavailable<T>() -> CoordResult<T> {
    Err(spot_coordinator::Error::new(
        UNAVAILABLE,
        "the coordination server is unreachable",
    ))
}

impl Coordinator for Cuttable {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, CoordResult<Lease>> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { unavailable() });
        }
        self.inner.acquire(name, holder, ttl)
    }
    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, CoordResult<Lease>> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { unavailable() });
        }
        self.inner.renew(lease, ttl)
    }
    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, CoordResult<()>> {
        self.inner.release(lease)
    }
    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<Option<Lease>>> {
        self.inner.current(name)
    }
    fn check_fence<'a>(
        &'a self,
        name: &'a str,
        token: FencingToken,
    ) -> BoxFuture<'a, CoordResult<()>> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { unavailable() });
        }
        self.inner.check_fence(name, token)
    }
    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.next(name, by)
    }
    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.peek(name)
    }
    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.set_at_least(name, value)
    }
    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, CoordResult<Admit>> {
        self.inner.take(key, limit, cost)
    }
    fn waker(&self) -> Arc<Notify> {
        self.inner.waker()
    }
    fn release_now(&self, lease: &Lease) -> bool {
        self.inner.release_now(lease)
    }
}

struct World {
    clock: Arc<ManualClock>,
    status: Arc<Status>,
    host: ObservedCoordinator,
    down: Arc<AtomicBool>,
    /// The shared store, for a second holder that is never cut.
    store: Arc<dyn Coordinator>,
}

async fn world() -> World {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let shared: Arc<dyn Coordinator> = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("store").unwrap(),
        Arc::clone(&clock) as Arc<dyn Clock>,
    )
    .await
    .unwrap();
    let down = Arc::new(AtomicBool::new(false));
    let status = Status::new();
    status.set_outage_policy(2, Duration::from_secs(10));
    let host = ObservedCoordinator::new(
        Arc::new(Cuttable {
            inner: Arc::clone(&shared),
            down: Arc::clone(&down),
        }),
        Arc::clone(&status),
        Arc::clone(&clock) as Arc<dyn Clock>,
    );
    World {
        clock,
        status,
        host,
        down,
        store: shared,
    }
}

const TTL: Duration = Duration::from_secs(30);

/// The monotonic reading the plugin's counter uses.
fn now_ms(world: &World) -> u64 {
    u64::try_from(world.clock.monotonic().as_millis()).unwrap()
}

#[tokio::test]
async fn an_outage_of_two_hundred_milliseconds_degrades_readiness_and_fences() {
    let w = world().await;
    let lease = w.host.acquire(LEASE, "host-a", TTL).await.unwrap();
    assert!(w.status.connected());

    // The cut lasts 200 ms, far inside one probe interval, and two calls hit it.
    w.down.store(true, Ordering::SeqCst);
    assert_eq!(
        w.host.renew(&lease, TTL).await.unwrap_err().code,
        UNAVAILABLE
    );
    w.clock.advance(Duration::from_millis(100));
    assert_eq!(
        w.host
            .check_fence(LEASE, lease.token)
            .await
            .unwrap_err()
            .code,
        UNAVAILABLE
    );
    w.clock.advance(Duration::from_millis(100));
    w.down.store(false, Ordering::SeqCst);

    // The server is back and answers, but the site is marked degraded: readiness says so and the
    // holder treats its lease as lost even though the server would renew it.
    assert!(w.status.marked());
    assert!(!w.status.connected());
    assert_eq!(w.status.short_outages(), 1);
    let reason = w.status.degraded_reason().unwrap();
    assert!(reason.contains("coordination server"), "{reason}");
    assert!(!reason.contains("redis://"), "no address in the reason");
    assert_eq!(
        w.host.renew(&lease, TTL).await.unwrap_err().code,
        UNAVAILABLE,
        "a marked site does not renew"
    );
    assert_eq!(
        w.host
            .check_fence(LEASE, lease.token)
            .await
            .unwrap_err()
            .code,
        UNAVAILABLE
    );
    assert_eq!(
        w.host
            .acquire("other", "host-a", TTL)
            .await
            .unwrap_err()
            .code,
        UNAVAILABLE
    );
    // The refusals above are not new failures: the mark does not extend itself.
    w.clock.advance(Duration::from_secs(11));
    assert!(
        w.status.probe_result(true, now_ms(&w)).1,
        "a good probe after a quiet window clears it"
    );
    assert!(!w.status.marked());
    assert!(w.status.connected());
    assert!(w.status.degraded_reason().is_none());
    // Back to normal: the holder may renew again.
    w.host.renew(&lease, TTL).await.unwrap();
}

#[tokio::test]
async fn a_single_failed_operation_does_not_degrade_anything() {
    let w = world().await;
    let lease = w.host.acquire(LEASE, "host-a", TTL).await.unwrap();
    w.down.store(true, Ordering::SeqCst);
    assert_eq!(
        w.host.renew(&lease, TTL).await.unwrap_err().code,
        UNAVAILABLE
    );
    w.down.store(false, Ordering::SeqCst);
    assert!(!w.status.marked());
    assert_eq!(w.status.short_outages(), 0);
    // The raw link stays down until the next answer; that answer proves the server is there.
    w.host.renew(&lease, TTL).await.unwrap();
    assert!(
        w.status.connected(),
        "the next answer proves the server is there"
    );

    // Two failures further apart than the window are two single failures.
    w.down.store(true, Ordering::SeqCst);
    w.clock.advance(Duration::from_secs(11));
    assert!(w.host.renew(&lease, TTL).await.is_err());
    assert!(!w.status.marked());
}

#[tokio::test]
async fn the_mark_stays_while_failures_keep_coming_and_clears_only_when_quiet() {
    let w = world().await;
    let lease = w.host.acquire(LEASE, "host-a", TTL).await.unwrap();
    w.down.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        assert!(w.host.renew(&lease, TTL).await.is_err());
    }
    w.down.store(false, Ordering::SeqCst);
    assert!(w.status.marked());

    // A probe before the window has passed does not clear it.
    w.clock.advance(Duration::from_secs(5));
    assert!(!w.status.probe_result(true, now_ms(&w)).1);
    assert!(w.status.marked());

    // A failure recorded inside the window (the cache gate saw one) keeps it marked later too.
    w.status.record_failure(now_ms(&w));
    w.clock.advance(Duration::from_secs(6));
    assert!(
        !w.status.probe_result(true, now_ms(&w)).1,
        "a failure 6 s ago is still inside the window"
    );
    w.clock.advance(Duration::from_secs(5));
    assert!(w.status.probe_result(true, now_ms(&w)).1);
    assert!(!w.status.marked());
}

#[tokio::test]
async fn a_business_refusal_never_counts() {
    let w = world().await;
    // Another holder has the lease: every acquire is refused as held, many times over.
    w.store.acquire(LEASE, "host-b", TTL).await.unwrap();
    for _ in 0..5 {
        let refused = w.host.acquire(LEASE, "host-a", TTL).await.unwrap_err();
        assert_eq!(refused.code, LEASE_HELD);
    }
    assert!(!w.status.marked());
    assert!(w.status.connected());
    assert_eq!(w.status.short_outages(), 0);
}

#[tokio::test]
async fn the_counter_is_bounded_however_long_an_outage_lasts() {
    let w = world().await;
    w.status.set_outage_policy(3, Duration::from_secs(10));
    // A thousand failures in the window keep the status healthy in memory terms: the counter
    // keeps at most the threshold of timestamps, and marks once.
    let mut marks = 0;
    for step in 0..1_000_u64 {
        if w.status.record_failure(step) {
            marks += 1;
        }
    }
    assert_eq!(marks, 1);
    assert_eq!(w.status.short_outages(), 1);
}

#[test]
fn the_settings_default_to_two_failures_in_ten_seconds_and_refuse_nonsense() {
    let defaults = Settings::default();
    assert_eq!(defaults.short_outage_failures, 2);
    assert_eq!(defaults.short_outage_window_ms, 10_000);
    let base = Settings {
        enabled: true,
        url: Some("redis://127.0.0.1:6379".to_owned()),
        allow_plaintext: true,
        ..Settings::default()
    };
    assert!(validate(&base).is_ok());
    for failures in [0_u32, 101] {
        let bad = Settings {
            short_outage_failures: failures,
            ..base.clone()
        };
        assert!(validate(&bad).is_err(), "{failures} failures");
    }
    for window in [0_u64, 99, 600_001] {
        let bad = Settings {
            short_outage_window_ms: window,
            ..base.clone()
        };
        assert!(validate(&bad).is_err(), "{window} ms");
    }
    // Unknown keys stay refused.
    assert!(toml::from_str::<Settings>("short_outage_failures = 3\nbogus = 1").is_err());
    let parsed: Settings =
        toml::from_str("short_outage_failures = 3\nshort_outage_window_ms = 5000").unwrap();
    assert_eq!(
        (parsed.short_outage_failures, parsed.short_outage_window_ms),
        (3, 5000)
    );
}
