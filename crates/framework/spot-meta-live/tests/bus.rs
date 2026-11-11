//! The invalidation buses: in-process delivery under load, the gap path, and the poll schedule
//! driven by the injected clock (WP-MET-03.T1, deliverables 3 and 5).

use std::time::Duration;

use spot_core::{ManualClock, SiteId};
use spot_meta_live::{
    BusEvent, InProcessBus, Invalidation, InvalidationBus, MemoryVersionStore, MetaHash,
    PollSchedule, PollingBus, Version, next_event,
};
use time::OffsetDateTime;

fn site() -> SiteId {
    SiteId::new("acme.example").unwrap_or_else(|e| panic!("valid site: {e}"))
}

/// A deterministic 64-hex content hash; the tests never parse it.
fn hash(seed: u8) -> MetaHash {
    let ch = char::from(b'a' + (seed % 26));
    MetaHash::from_hex(std::iter::repeat_n(ch, 64).collect::<String>())
}

#[tokio::test]
async fn the_in_process_bus_delivers_in_order_and_drops_nothing() {
    let bus = InProcessBus::new(4096);
    let site = site();
    let mut stream = bus.subscribe(&site);
    assert_eq!(bus.subscribers(&site), 1);

    for i in 1..=1000u64 {
        bus.publish(&site, Invalidation::meta(Version::new(i), hash(1)))
            .await
            .unwrap_or_else(|e| panic!("publish {i}: {e}"));
    }

    for i in 1..=1000u64 {
        match next_event(&mut stream).await {
            Some(BusEvent::Change(invalidation)) => {
                assert_eq!(
                    invalidation.version,
                    Version::new(i),
                    "delivery is in order"
                );
            }
            other => panic!("expected change {i}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_overflowed_subscriber_reads_a_gap() {
    // Capacity one: the second publish overflows the queue and the subscriber is told to
    // reconcile rather than shown a silently incomplete stream.
    let bus = InProcessBus::new(1);
    let site = site();
    let mut stream = bus.subscribe(&site);
    for i in 1..=3u64 {
        bus.publish(&site, Invalidation::meta(Version::new(i), hash(i as u8)))
            .await
            .unwrap_or_else(|e| panic!("publish {i}: {e}"));
    }
    match next_event(&mut stream).await {
        Some(BusEvent::Gap) => {}
        other => panic!("expected a gap, got {other:?}"),
    }
}

#[tokio::test]
async fn the_polling_bus_writes_the_metadata_head() {
    let store = std::sync::Arc::new(MemoryVersionStore::new());
    let bus = PollingBus::new(store.clone());
    let site = site();
    bus.publish(&site, Invalidation::meta(Version::new(7), hash(7)))
        .await
        .unwrap_or_else(|e| panic!("publish: {e}"));
    assert_eq!(store.head(), Some((Version::new(7), hash(7))));
    assert_eq!(
        bus.poll_once()
            .await
            .unwrap_or_else(|e| panic!("poll: {e}")),
        Some((Version::new(7), hash(7)))
    );
}

#[test]
fn the_poll_schedule_uses_the_injected_clock() {
    let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH);
    let schedule = PollSchedule::new(Duration::from_secs(30));
    assert!(schedule.due(&clock), "the first call reconciles at once");
    assert!(!schedule.due(&clock), "not again before the interval");
    clock.advance(Duration::from_secs(30));
    assert!(schedule.due(&clock));
    assert!(!schedule.due(&clock));
    clock.advance(Duration::from_secs(29));
    assert!(!schedule.due(&clock));
    clock.advance(Duration::from_secs(1));
    assert!(schedule.due(&clock));
}
