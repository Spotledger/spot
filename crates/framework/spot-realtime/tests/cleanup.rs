//! Cleanup: rooms are freed when the last subscriber leaves and the replay buffer is gone;
//! `stats()` reports it, and no per-subscription state leaks.

use std::sync::Arc;
use std::time::Duration;

use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use spot_realtime::{AllowAll, BusConfig, BusStats, Principal, RealtimeBus, Room};

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("test site name should be valid: {e}"))
}

fn room(name: &str) -> Room {
    Room::doctype(name).unwrap_or_else(|e| panic!("valid room: {e}"))
}

#[tokio::test]
async fn an_idle_bus_holds_nothing() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    assert_eq!(
        bus.stats(),
        BusStats {
            rooms: 0,
            subscriptions: 0
        }
    );
}

#[tokio::test]
async fn room_count_returns_to_zero_after_all_subscriptions_drop() {
    // A manual clock makes "replay ages out" deterministic: after the drop the room is
    // held only by its replay buffer, and once that expires the bus must be empty again.
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let bus = RealtimeBus::new(
        BusConfig {
            replay_max_age: Duration::from_secs(60),
            ..BusConfig::default()
        },
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let a = room("A");
    let b = room("B");

    {
        let mut sub1 = bus.subscribe(site.clone(), Principal::user("alice"));
        let mut sub2 = bus.subscribe(site.clone(), Principal::user("bob"));
        sub1.join(&a, None)
            .await
            .unwrap_or_else(|e| panic!("join: {e}"));
        sub1.join(&b, None)
            .await
            .unwrap_or_else(|e| panic!("join: {e}"));
        sub2.join(&a, None)
            .await
            .unwrap_or_else(|e| panic!("join: {e}"));
        bus.publish(&site, &a, "x", serde_json::json!(null));
        bus.publish(&site, &b, "y", serde_json::json!(null));
        let stats = bus.stats();
        assert_eq!(stats.rooms, 2, "both rooms live while subscribed");
        assert_eq!(stats.subscriptions, 2);
    }
    // Both subscriptions dropped: membership is gone; the rooms survive only as replay.
    let after_drop = bus.stats();
    assert_eq!(after_drop.subscriptions, 0, "no subscription state leaks");
    assert_eq!(after_drop.rooms, 2, "replay still holds the rooms briefly");

    clock.advance(Duration::from_secs(61));
    assert_eq!(
        bus.stats().rooms,
        0,
        "after the replay window the rooms are freed"
    );
}

#[tokio::test]
async fn an_idle_room_is_swept_out_by_publish_traffic_alone() {
    // The bounded amortised sweep (spot-cache's pattern): no stats() call anywhere here —
    // ordinary publishes to another room must age and free an idle room by themselves,
    // observable through the recreated room's new seed (see the crate docs).
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let idle = room("Idle");
    let busy = room("Busy");

    // "Idle" is this bus's first room: nothing has been handed out yet, so its first
    // publish is seq 1. "Busy" is created by its own first publish instead.
    assert_eq!(bus.publish(&site, &idle, "x", serde_json::json!(1)), 1);
    clock.advance(Duration::from_secs(6 * 60)); // "Idle"'s replay is past replay_max_age

    // Enough publishes to "Busy" to cross the sweep interval and reach "Idle"'s window.
    for i in 0..spot_realtime::SWEEP_INTERVAL_PUBLISHES {
        bus.publish(&site, &busy, "keepalive", serde_json::json!(i));
    }

    // "Idle" must be gone: its next publish recreates it, seeded above the bus's
    // high-water mark ("Busy"'s N publishes handed out exactly the seqs 2..=1+N), so
    // 2+N. Had the sweep not freed it, the seq would have continued at 2.
    let again = bus.publish(&site, &idle, "x", serde_json::json!(2));
    assert_eq!(
        again,
        2 + spot_realtime::SWEEP_INTERVAL_PUBLISHES,
        "the amortised sweep must have freed the idle room without a stats() call, got seq {again}"
    );
}

#[tokio::test]
async fn dropping_one_of_two_subscribers_keeps_the_room_for_the_other() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");
    let mut a = bus.subscribe(site.clone(), Principal::user("a"));
    let mut b = bus.subscribe(site.clone(), Principal::user("b"));
    a.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    b.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(bus.stats().rooms, 1);
    drop(a);
    let stats = bus.stats();
    assert_eq!(
        stats.subscriptions, 1,
        "the dropped subscription is fully removed"
    );
    assert_eq!(
        stats.rooms, 1,
        "the room lives on for the remaining subscriber"
    );
}

#[tokio::test]
async fn leaving_the_last_membership_of_an_unpublished_room_frees_it_immediately() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");
    let mut sub = bus.subscribe(site, Principal::user("alice"));
    sub.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(bus.stats().rooms, 1);
    sub.leave(&feed);
    assert_eq!(
        bus.stats().rooms,
        0,
        "a room with no subscriber and no replay is freed at once"
    );
    // Leaving again is a harmless no-op.
    sub.leave(&feed);
    assert_eq!(bus.stats().rooms, 0);
}
