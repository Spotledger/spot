//! `PendingEvents`: the after-commit buffer — flush publishes in order, drop publishes
//! nothing (the rollback path).

use std::sync::Arc;
use std::time::Duration;

use spot_core::{SiteId, SystemClock};
use spot_realtime::{
    AllowAll, BusConfig, Event, PendingEvents, Principal, RealtimeBus, Room, Subscription,
};

/// The bus's seq assignment: rooms joined before any publish each count their own seqs
/// from 1 (the counter is seeded at the bus's high-water mark — see the crate docs), and
/// independent per-(site, room) counters never alias each other.
fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("test site name should be valid: {e}"))
}

fn room(name: &str) -> Room {
    Room::doctype(name).unwrap_or_else(|e| panic!("valid room: {e}"))
}

async fn recv(sub: &mut Subscription) -> Option<Event> {
    tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .ok()
        .and_then(|res| res.ok())
}

#[tokio::test]
async fn a_flushed_buffer_publishes_its_events_in_insertion_order() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let a = room("A");
    let b = room("B");

    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    sub.join(&a, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    sub.join(&b, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));

    let mut pending = PendingEvents::new();
    assert!(pending.is_empty());
    pending.push(&site, &a, "doc_updated", serde_json::json!("one"));
    pending.push(&site, &b, "doc_updated", serde_json::json!("two"));
    pending.push(&site, &a, "doc_updated", serde_json::json!("three"));
    assert_eq!(pending.len(), 3);
    // Nothing published yet: the subscriber must see nothing before the flush.
    assert!(
        recv(&mut sub).await.is_none(),
        "buffered events are invisible pre-commit"
    );

    let seqs = pending.flush(&bus);
    // Room A is this bus's first created room, so its first flush event is seq 1 — and
    // the rollbacks never burned a seq, so its second is 2. Room B was joined before any
    // publish: its flush event counts its own 1, independent of A's counter.
    assert_eq!(seqs, vec![1, 1, 2]);
    assert!(pending.is_empty(), "flush drains the buffer");

    let e1 = recv(&mut sub)
        .await
        .unwrap_or_else(|| panic!("first flush event"));
    assert_eq!(
        (e1.room, e1.seq, e1.payload.as_ref()),
        (a.clone(), 1, &serde_json::json!("one"))
    );
    let e2 = recv(&mut sub)
        .await
        .unwrap_or_else(|| panic!("second flush event"));
    assert_eq!((e2.room, e2.seq), (b.clone(), 1));
    let e3 = recv(&mut sub)
        .await
        .unwrap_or_else(|| panic!("third flush event"));
    assert_eq!((e3.room, e3.seq), (a, 2));
}

#[tokio::test]
async fn a_dropped_buffer_publishes_nothing() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");
    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    sub.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));

    {
        // Simulated rollback: the buffer leaves scope unflushed.
        let mut pending = PendingEvents::new();
        pending.push(&site, &feed, "doc_updated", serde_json::json!("ghost"));
    }

    assert!(
        recv(&mut sub).await.is_none(),
        "a rolled-back transaction publishes nothing"
    );
    assert_eq!(
        bus.stats().rooms,
        1,
        "the room exists only because `sub` is still joined, not because of the ghost event"
    );

    // The explicit discard path behaves the same.
    let mut pending = PendingEvents::new();
    pending.push(&site, &feed, "doc_updated", serde_json::json!("also-ghost"));
    pending.discard();
    assert!(pending.is_empty());
    assert!(recv(&mut sub).await.is_none());

    // And a real commit after rollbacks still publishes with the right seq (nothing
    // consumed a seq number during the rollbacks).
    let mut pending = PendingEvents::new();
    pending.push(&site, &feed, "doc_updated", serde_json::json!("real"));
    assert_eq!(
        pending.flush(&bus),
        vec![1],
        "rolled-back events never burned a seq"
    );
    let event = recv(&mut sub)
        .await
        .unwrap_or_else(|| panic!("committed event"));
    assert_eq!(event.payload.as_ref(), &serde_json::json!("real"));
}
