//! `RealtimeBus::evict_room` (WP-PLT-08.T5): the host makes one principal's subscriptions
//! leave one room, silently, with no event of that room delivered afterwards.

use std::sync::Arc;
use std::time::Duration;

use spot_core::{SiteId, SystemClock};
use spot_realtime::{AllowAll, BusConfig, Principal, RealtimeBus, Room};

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("test site name should be valid: {e}"))
}

fn room(name: &str) -> Room {
    Room::doctype(name).unwrap_or_else(|e| panic!("valid room: {e}"))
}

fn bus() -> RealtimeBus {
    RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    )
}

async fn next_kind(sub: &mut spot_realtime::Subscription) -> String {
    tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .unwrap_or_else(|_| panic!("no event within 5s"))
        .unwrap_or_else(|e| panic!("subscription closed: {e:?}"))
        .kind
}

async fn assert_silent(sub: &mut spot_realtime::Subscription) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sub.recv())
            .await
            .is_err(),
        "no event should be delivered"
    );
}

#[tokio::test]
async fn eviction_removes_only_that_principals_subscriptions_from_that_room() {
    let bus = bus();
    let s = site("acme.example");
    let (a, b) = (Principal::user("a"), Principal::user("b"));
    let (r, other) = (room("R"), room("Other"));
    let mut a1 = bus.subscribe(s.clone(), a.clone());
    let mut a2 = bus.subscribe(s.clone(), a.clone());
    let mut b1 = bus.subscribe(s.clone(), b.clone());
    for sub in [&mut a1, &mut a2, &mut b1] {
        sub.join(&r, None).await.unwrap_or_else(|e| panic!("{e:?}"));
    }
    a1.join(&other, None)
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));

    assert_eq!(bus.evict_room(&s, &a, &r), 2);
    assert_eq!(bus.evict_room(&s, &a, &r), 0, "idempotent");

    bus.publish(&s, &r, "to_r", serde_json::json!({}));
    bus.publish(&s, &other, "to_other", serde_json::json!({}));
    assert_eq!(next_kind(&mut b1).await, "to_r");
    // A's connections no longer see R but keep their other rooms.
    assert_eq!(next_kind(&mut a1).await, "to_other");
    assert_silent(&mut a2).await;
    assert_silent(&mut a1).await;
}

#[tokio::test]
async fn an_event_queued_before_the_eviction_is_not_delivered() {
    let bus = bus();
    let s = site("acme.example");
    let a = Principal::user("a");
    let (r, other) = (room("R"), room("Other"));
    let mut sub = bus.subscribe(s.clone(), a.clone());
    sub.join(&r, None).await.unwrap_or_else(|e| panic!("{e:?}"));
    sub.join(&other, None)
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));
    bus.publish(&s, &r, "queued_r", serde_json::json!({}));
    bus.publish(&s, &other, "queued_other", serde_json::json!({}));
    assert_eq!(bus.evict_room(&s, &a, &r), 1);
    assert_eq!(next_kind(&mut sub).await, "queued_other");
    assert_silent(&mut sub).await;
}

#[tokio::test]
async fn another_site_with_the_same_room_name_is_untouched() {
    let bus = bus();
    let (s1, s2) = (site("one.example"), site("two.example"));
    let a = Principal::user("a");
    let r = room("R");
    let mut on_one = bus.subscribe(s1.clone(), a.clone());
    let mut on_two = bus.subscribe(s2.clone(), a.clone());
    on_one
        .join(&r, None)
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));
    on_two
        .join(&r, None)
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(bus.evict_room(&s1, &a, &r), 1);
    bus.publish(&s2, &r, "two", serde_json::json!({}));
    assert_eq!(next_kind(&mut on_two).await, "two");
    bus.publish(&s1, &r, "one", serde_json::json!({}));
    assert_silent(&mut on_one).await;
}

#[tokio::test]
async fn the_principal_match_is_exact() {
    let bus = bus();
    let s = site("acme.example");
    let r = room("R");
    let user = Principal::user("guest");
    let guest = Principal::guest(); // same id "guest", guest flag set
    let mut sub = bus.subscribe(s.clone(), user.clone());
    sub.join(&r, None).await.unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(bus.evict_room(&s, &guest, &r), 0);
    assert_eq!(bus.evict_room(&s, &Principal::user("Guest"), &r), 0);
    bus.publish(&s, &r, "still", serde_json::json!({}));
    assert_eq!(next_kind(&mut sub).await, "still");
}

#[tokio::test]
async fn unknown_site_room_or_principal_returns_zero_without_panicking() {
    let bus = bus();
    let s = site("acme.example");
    let a = Principal::user("a");
    assert_eq!(bus.evict_room(&s, &a, &room("Nope")), 0);
    let mut sub = bus.subscribe(s.clone(), a.clone());
    sub.join(&room("R"), None)
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(bus.evict_room(&site("other.example"), &a, &room("R")), 0);
    assert_eq!(bus.evict_room(&s, &Principal::user("z"), &room("R")), 0);
    // In another room only: not in `Nope`.
    assert_eq!(bus.evict_room(&s, &a, &room("Nope")), 0);
}

#[tokio::test]
async fn the_room_is_freed_when_its_last_subscriber_is_evicted_and_can_be_rejoined() {
    let bus = bus();
    let s = site("acme.example");
    let a = Principal::user("a");
    let r = room("R");
    let mut sub = bus.subscribe(s.clone(), a.clone());
    sub.join(&r, None).await.unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(bus.stats().rooms, 1);
    assert_eq!(bus.evict_room(&s, &a, &r), 1);
    // No replay was published, so nothing keeps the room.
    assert_eq!(bus.stats().rooms, 0);
    assert_eq!(bus.stats().subscriptions, 1, "the subscription stays open");
    sub.join(&r, None).await.unwrap_or_else(|e| panic!("{e:?}"));
    bus.publish(&s, &r, "again", serde_json::json!({}));
    assert_eq!(next_kind(&mut sub).await, "again");
}

/// 8 tasks publish, join and evict at once on a multi-threaded runtime: no deadlock (bounded
/// time), and once an eviction has returned with no rejoin after it, nothing of the room
/// arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_publish_join_and_evict_neither_deadlocks_nor_leaks_events() {
    let bus = bus();
    let s = site("acme.example");
    let a = Principal::user("a");
    let r = room("R");
    let mut sub = bus.subscribe(s.clone(), a.clone());
    sub.join(&r, None).await.unwrap_or_else(|e| panic!("{e:?}"));

    let mut tasks = Vec::new();
    for t in 0..8usize {
        let (bus, s, a, r) = (bus.clone(), s.clone(), a.clone(), r.clone());
        tasks.push(tokio::spawn(async move {
            for i in 0..300 {
                match t % 3 {
                    0 => {
                        bus.publish(&s, &r, "e", serde_json::json!({ "i": i }));
                    }
                    1 => {
                        bus.evict_room(&s, &a, &r);
                    }
                    _ => {
                        let mut extra = bus.subscribe(s.clone(), a.clone());
                        let _ = extra.join(&r, None).await;
                        bus.evict_room(&s, &a, &r);
                    }
                }
            }
        }));
    }
    for task in tasks {
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .unwrap_or_else(|_| panic!("a task hung: deadlock"))
            .unwrap_or_else(|e| panic!("task failed: {e}"));
    }
    // Evict for good, then publish: nothing may arrive, buffered events included.
    bus.evict_room(&s, &a, &r);
    bus.publish(&s, &r, "after", serde_json::json!({}));
    if let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(100), sub.recv()).await {
        panic!("event {} delivered after eviction", event.kind);
    }
}
