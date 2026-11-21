//! Ordering: per-room `seq` order, multi-room delivery, per-site isolation.

use std::sync::Arc;
use std::time::Duration;

use spot_core::{SiteId, SystemClock};
use spot_realtime::{AllowAll, BusConfig, Principal, RealtimeBus, Room, Subscription};

/// The bus's seq assignment: a room's counter is seeded at the bus's current high-water
/// mark (see the `spot_realtime` crate docs), so the second-ever room of a bus that has
/// already handed out seqs still counts up from strictly above everything handed out —
/// and two rooms joined before any publish both count their own seqs from 1.
fn bus() -> RealtimeBus {
    RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    )
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("test site name should be valid: {e}"))
}

/// Awaits `recv` with a safety timeout: a bus bug fails the test instead of hanging CI.
async fn recv(sub: &mut Subscription) -> Result<spot_realtime::Event, &'static str> {
    match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        Ok(Ok(event)) => Ok(event),
        Ok(Err(closed)) => Err(panic_closed(closed)),
        Err(_) => Err("recv timed out: an event was never delivered"),
    }
}

fn panic_closed(closed: spot_realtime::Closed) -> &'static str {
    panic!("subscription closed unexpectedly: {closed}")
}

#[tokio::test]
async fn events_arrive_in_seq_order_within_a_room() {
    let bus = bus();
    let site = site("acme.example");
    let room = Room::doctype("Item").unwrap_or_else(|e| panic!("valid room: {e}"));
    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    sub.join(&room, None)
        .await
        .unwrap_or_else(|e| panic!("join should succeed: {e}"));

    for i in 1..=50u64 {
        let seq = bus.publish(&site, &room, "doc_updated", serde_json::json!({ "n": i }));
        assert_eq!(seq, i, "seq starts at 1 and increases by one per event");
    }
    for i in 1..=50u64 {
        let event = recv(&mut sub)
            .await
            .unwrap_or_else(|why| panic!("event {i} should arrive: {why}"));
        assert_eq!(event.seq, i, "per-room delivery is seq-ordered");
    }
}

#[tokio::test]
async fn one_subscription_receives_events_from_every_joined_room() {
    let bus = bus();
    let site = site("acme.example");
    let doc = Room::document("Item", "it-1").unwrap_or_else(|e| panic!("valid room: {e}"));
    let feed = Room::doctype("Item").unwrap_or_else(|e| panic!("valid room: {e}"));
    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    sub.join(&doc, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    sub.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));

    // Both rooms were joined before any publish, so each counts its own seqs from 1:
    // per-(site, room) counters stay independent, and no seq aliases another room's.
    bus.publish(&site, &doc, "doc_updated", serde_json::json!(1));
    let feed_seq = bus.publish(&site, &feed, "doctype_updated", serde_json::json!(2));

    let mut got = Vec::new();
    for _ in 0..2 {
        let event = recv(&mut sub)
            .await
            .unwrap_or_else(|why| panic!("both rooms deliver: {why}"));
        got.push((event.room.as_str().to_string(), event.seq));
    }
    assert!(
        got.contains(&("doc:Item:it-1".to_string(), 1)),
        "doc room event missing: {got:?}"
    );
    assert!(
        got.contains(&("doctype:Item".to_string(), feed_seq)),
        "feed room event missing: {got:?}"
    );
}

#[tokio::test]
async fn the_same_room_name_on_two_sites_is_two_unrelated_rooms() {
    let bus = bus();
    let site_a = site("acme.example");
    let site_b = site("other.example");
    let room = Room::doctype("Item").unwrap_or_else(|e| panic!("valid room: {e}"));
    let mut sub_a = bus.subscribe(site_a.clone(), Principal::user("alice"));
    let mut sub_b = bus.subscribe(site_b.clone(), Principal::user("bob"));
    sub_a
        .join(&room, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    sub_b
        .join(&room, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));

    // Site A gets two events; site B's identically named room is on its own seq counter.
    assert_eq!(
        bus.publish(&site_a, &room, "a1", serde_json::json!(null)),
        1
    );
    assert_eq!(
        bus.publish(&site_a, &room, "a2", serde_json::json!(null)),
        2
    );
    assert_eq!(
        bus.publish(&site_b, &room, "b1", serde_json::json!(null)),
        1,
        "each (site, room) runs its own counter; B's was joined before any publish"
    );

    let e = recv(&mut sub_a)
        .await
        .unwrap_or_else(|why| panic!("site A event: {why}"));
    assert_eq!((e.kind.as_str(), e.seq), ("a1", 1));
    let e = recv(&mut sub_a)
        .await
        .unwrap_or_else(|why| panic!("site A event: {why}"));
    assert_eq!((e.kind.as_str(), e.seq), ("a2", 2));

    // Site B's subscriber sees only B's event; nothing from A crosses sites.
    let e = tokio::time::timeout(Duration::from_secs(5), sub_b.recv())
        .await
        .map_or_else(
            |_| panic!("site B should get its own event"),
            |res| res.unwrap_or_else(|c| panic!("closed: {c}")),
        );
    assert_eq!((e.kind.as_str(), e.seq), ("b1", 1));
    let extra = tokio::time::timeout(Duration::from_millis(100), sub_b.recv())
        .await
        .is_ok_and(|res| res.is_ok());
    assert!(!extra, "site A events must not leak into site B");
}

/// The scaling probe the review called for: many rooms created in sequence, each after
/// the bus has handed seqs out, must every hand out exactly one above the high-water
/// mark — no saturation cliff, no aliasing between rooms.
#[tokio::test]
async fn room_creations_keep_seeding_above_every_seq_ever_handed_out() {
    let bus = bus();
    let site = site("acme.example");
    let first = Room::doctype("First").unwrap_or_else(|e| panic!("valid room: {e}"));
    assert_eq!(
        bus.publish(&site, &first, "kick", serde_json::json!(0)),
        1,
        "the bus's very first room counts from seq 1"
    );
    let mut last = 1u64;
    for i in 0..300u64 {
        let room = Room::doctype(&format!("Room{i}")).unwrap_or_else(|e| panic!("valid room: {e}"));
        let seq = bus.publish(&site, &room, "kick", serde_json::json!(i));
        assert_eq!(
            seq,
            last + 1,
            "every new room hands out exactly one above the bus's high-water mark"
        );
        last = seq;
    }
    assert_eq!(
        last, 301,
        "300 more rooms, each seeded strictly above the last handout"
    );
}
