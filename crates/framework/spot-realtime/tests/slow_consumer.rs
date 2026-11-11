//! Backpressure: a full queue closes that subscriber — nobody else waits or loses events.

use std::sync::Arc;
use std::time::{Duration, Instant};

use spot_core::{SiteId, SystemClock};
use spot_realtime::{
    AllowAll, BusConfig, Closed, Event, JoinError, Principal, RealtimeBus, Room, Subscription,
};

/// The bus's seq assignment: rooms joined before any publish each count their own seqs
/// from 1 (the counter is seeded at the bus's high-water mark — see the crate docs), and
/// one room's seqs never touch another room's.
fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("test site name should be valid: {e}"))
}

fn room(name: &str) -> Room {
    Room::doctype(name).unwrap_or_else(|e| panic!("valid room: {e}"))
}

/// Awaits `recv` with a safety timeout, failing the test instead of hanging CI.
async fn recv(sub: &mut Subscription) -> Event {
    tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .map_or_else(
            |_| panic!("recv timed out"),
            |res| res.unwrap_or_else(|c| panic!("subscription closed unexpectedly: {c}")),
        )
}

#[tokio::test]
async fn a_full_queue_closes_only_that_subscriber_as_slow_consumer() {
    let capacity = 4;
    let bus = RealtimeBus::new(
        BusConfig {
            queue_capacity: capacity,
            ..BusConfig::default()
        },
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");

    let mut slow = bus.subscribe(site.clone(), Principal::user("slow"));
    let mut fast = bus.subscribe(site.clone(), Principal::user("fast"));
    slow.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    fast.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));

    // capacity + 1 events. `fast` reads one per publish so its queue never fills;
    // `slow` never reads and its queue overflows on the last publish.
    for i in 1..=(capacity + 1) as u64 {
        bus.publish(&site, &feed, "e", serde_json::json!(i));
        let event = recv(&mut fast).await;
        assert_eq!(
            event.seq, i,
            "unaffected subscriber keeps every event in order"
        );
    }

    let closed = tokio::time::timeout(Duration::from_secs(5), slow.recv())
        .await
        .unwrap_or_else(|_| panic!("close must be reported"))
        .err_or_closed("slow subscriber must be closed");
    assert_eq!(closed, Closed::SlowConsumer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_thousand_publishes_finish_while_one_subscriber_never_reads() {
    // The brief's measurement: fill one subscriber's queue, publish 10,000 events while it
    // never reads, and show publishers are never blocked and the other subscriber still
    // receives every event. The healthy subscriber is drained between publishes (one recv
    // per publish keeps its queue below capacity without racing a spawned task).
    const N: u64 = 10_000;
    let bus = RealtimeBus::new(
        BusConfig {
            queue_capacity: 64,
            ..BusConfig::default()
        },
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");

    let mut stuck = bus.subscribe(site.clone(), Principal::user("stuck"));
    let mut healthy = bus.subscribe(site.clone(), Principal::user("healthy"));
    stuck
        .join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    healthy
        .join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));

    let started = Instant::now();
    let mut seen = Vec::with_capacity(N as usize);
    for i in 1..=N {
        assert_eq!(
            bus.publish(&site, &feed, "doc_updated", serde_json::json!(i)),
            i
        );
        // Drain the reading subscriber so its queue stays under capacity; the stuck one
        // overflows at event 65 and is closed by the bus.
        if let Some(event) = rx_now(&mut healthy).await {
            seen.push(event);
        }
    }
    let elapsed = started.elapsed();
    // A publish that waited for the stuck subscriber would block forever; the generous
    // bound only catches a bus that "fixed" backpressure by sleeping or retrying.
    assert!(
        elapsed < Duration::from_secs(30),
        "10,000 publishes took {elapsed:?} — publish must never wait for a subscriber"
    );
    println!("WP-PLT-08.T1: {N} publishes with one never-reading subscriber: {elapsed:?}");

    // Drain whatever the healthy subscriber still holds.
    while let Some(event) = rx_now(&mut healthy).await {
        seen.push(event);
    }
    let expected: Vec<u64> = (1..=N).collect();
    let got: Vec<u64> = seen.iter().map(|e| e.seq).collect();
    assert_eq!(
        got, expected,
        "the reading subscriber receives every event, gap-free"
    );

    // The stuck subscriber gets exactly one answer: SlowConsumer.
    let closed = tokio::time::timeout(Duration::from_secs(5), stuck.recv())
        .await
        .unwrap_or_else(|_| panic!("close must be reported"))
        .err_or_closed("the never-reading subscriber must be closed");
    assert_eq!(closed, Closed::SlowConsumer);
}

#[tokio::test]
async fn publishers_and_other_rooms_are_unaffected_by_a_closed_subscriber() {
    let bus = RealtimeBus::new(
        BusConfig {
            queue_capacity: 1,
            ..BusConfig::default()
        },
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let a = room("A");
    let b = room("B");
    let mut doomed = bus.subscribe(site.clone(), Principal::user("doomed"));
    let mut other_room = bus.subscribe(site.clone(), Principal::user("other"));
    doomed
        .join(&a, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    other_room
        .join(&b, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));

    bus.publish(&site, &a, "1", serde_json::json!(null));
    bus.publish(&site, &a, "2", serde_json::json!(null)); // overflow -> doomed closes
    let seq = bus.publish(&site, &a, "3", serde_json::json!(null));
    assert_eq!(
        seq, 3,
        "publish keeps counting after closing a slow consumer"
    );
    let closed = tokio::time::timeout(Duration::from_secs(5), doomed.recv())
        .await
        .unwrap_or_else(|_| panic!("close must be reported"))
        .err_or_closed("doomed must report closing");
    assert_eq!(closed, Closed::SlowConsumer);

    // A subscriber on a different room is untouched: B was joined before any publish, so
    // its first event counts its own seq from 1 — room A's seqs never touch it.
    bus.publish(&site, &b, "x", serde_json::json!(null));
    let event = recv(&mut other_room).await;
    assert_eq!((event.room, event.seq), (b, 1));
}

#[tokio::test]
async fn a_replay_that_does_not_fit_the_queue_closes_the_join() {
    let capacity = 1;
    let bus = RealtimeBus::new(
        BusConfig {
            queue_capacity: capacity,
            ..BusConfig::default()
        },
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");

    // Events pile up in the replay buffer while nobody is connected.
    for i in 1..=3u64 {
        bus.publish(&site, &feed, "doc_updated", serde_json::json!(i));
    }

    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    // A resume at seq 1 must replay 2, 3 — into a one-slot queue. The replay cannot fit,
    // so the join refuses outright instead of half-joining and closing on the first recv.
    let err = tokio::time::timeout(Duration::from_secs(5), sub.join(&feed, Some(1)))
        .await
        .unwrap_or_else(|_| panic!("join should answer, not hang"))
        .err_or_closed("an overflowing replay join must be refused");
    assert_eq!(
        err,
        JoinError::Closed,
        "the subscription is a dead slow consumer"
    );

    // It stays dead: the follow-up join on the same subscription is rejected too.
    let err = tokio::time::timeout(Duration::from_secs(5), sub.join(&feed, Some(1)))
        .await
        .unwrap_or_else(|_| panic!("join should answer, not hang"))
        .err_or_closed("a closed subscription cannot join again");
    assert_eq!(err, JoinError::Closed);
}

/// Unwraps the `Err` side, panicking with `why` when it is `Ok`.
trait ErrOrClosed<T, E> {
    fn err_or_closed(self, why: &str) -> E;
}

impl<T, E> ErrOrClosed<T, E> for Result<T, E> {
    fn err_or_closed(self, why: &str) -> E {
        match self {
            Err(error) => error,
            Ok(_) => panic!("{why}: expected an error, got Ok"),
        }
    }
}

/// Non-blocking drain: `recv` with a zero timeout. Tokio polls the inner future first, so
/// this returns an already-buffered event without waiting; `None` means the queue is empty.
async fn rx_now(sub: &mut Subscription) -> Option<Event> {
    tokio::time::timeout(Duration::ZERO, sub.recv())
        .await
        .ok()
        .and_then(|res| res.ok())
}
