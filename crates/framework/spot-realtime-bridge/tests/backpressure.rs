//! The backpressure suite (WP-PLT-08.T3): a stalled subscriber with a full queue is
//! disconnected with the documented close code and delays no one; the bus carries the
//! wire close code the transport maps (`Closed::SlowConsumer` → 4008, the contract's
//! slow-consumer close); the pump's publish path never awaits a subscriber; the lag the
//! pump trails its feed head by is recorded through the telemetry seam.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use spot_realtime::{AllowAll, Closed, Room, Subscription};
use spot_realtime_bridge::FeedPump;

use support::{
    FakeFeed, FakeStore, FakeTelemetry, Must, TelemetryRecord, bus_with, changed_fields_payload,
    doc_id_text, empty_data_payload, identity_payload, pump, site, user_text, watcher,
};

const SUBSCRIBERS: usize = 1_000;
/// Index of the one subscriber that never reads (the stalled one).
const STALLED: usize = 7;

fn site1() -> spot_core::SiteId {
    site("acme.example")
}

/// A drain task: reads exactly `expected` events (the test hands in the count both
/// batches must deliver); a close as a slow consumer before that is reported through the
/// flag and ends the task with what arrived. The safety timeout turns a stuck delivery
/// into a test failure instead of a hang.
async fn drain_task(
    mut sub: Subscription,
    expected: usize,
    closed: Arc<AtomicUsize>,
    received: Arc<AtomicUsize>,
) -> usize {
    let mut got = 0;
    while got < expected {
        match tokio::time::timeout(Duration::from_secs(30), sub.recv()).await {
            Ok(Ok(_)) => {
                got += 1;
                received.fetch_add(1, Ordering::SeqCst);
            }
            Ok(Err(Closed::SlowConsumer)) => {
                closed.fetch_add(1, Ordering::SeqCst);
                break;
            }
            Ok(Err(Closed::ChannelClosed)) => break,
            Err(_) => panic!("a drain task stalled: no event within 30s"),
        }
    }
    got
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_stalled_subscriber_among_1000_is_closed_and_delays_no_one() {
    // Capacity 100: a healthy subscriber gets 65 events per batch, which always fits, so no
    // healthy queue can overflow by scheduling luck (the test once used 64, one slot short,
    // and failed on loaded runners). The stalled subscriber also follows the document room,
    // so it gets 130 events in the first batch and overflows structurally.
    let bus = bus_with(100, Arc::new(AllowAll));
    let site = site1();
    let doctype_room = Room::doctype("sales_invoice").must();
    let doc_room = Room::document("sales_invoice", &doc_id_text(42)).must();

    // 1,000 subscribers on one doctype room; subscriber 7 never reads.
    let mut subs: Vec<Subscription> = Vec::with_capacity(SUBSCRIBERS);
    for id in 0..SUBSCRIBERS {
        let sub = watcher(&bus, &site, &format!("sub-{id}"));
        subs.push(sub);
    }
    for sub in &mut subs {
        sub.join(&doctype_room, None).await.must();
    }
    subs[STALLED].join(&doc_room, None).await.must();

    // The pump and its collaborators, exactly as the host wires them.
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());
    let p: FeedPump<FakeFeed, FakeTelemetry> =
        pump(&site, &feed, &bus, &store, &telemetry, 100, true);

    // 65 distinct changes: each reaches the room twice (doc and doctype) through the
    // pump's mapping — 130 events, more than the stalled queue's 64 slots.
    for seq in 1..=65u64 {
        feed.push_change(
            seq,
            "sales_invoice",
            42,
            spot_storage_contract::ChangeOp::Insert,
            1,
        );
    }

    // Move the 999 healthy subscriptions into drain tasks; keep the stalled one out of
    // any task (its close is asserted separately). The tasks read concurrently while
    // the pump publishes; the publish pass is one synchronous, never-awaiting walk —
    // if the pump (or the bus) waited for subscriber 7, it could not finish.
    let closed = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::with_capacity(SUBSCRIBERS - 1);
    let mut stalled_sub: Option<Subscription> = None;
    for (id, sub) in subs.into_iter().enumerate() {
        if id == STALLED {
            stalled_sub = Some(sub);
            continue;
        }
        tasks.push(tokio::spawn(drain_task(
            sub,
            130,
            Arc::clone(&closed),
            Arc::clone(&received),
        )));
    }
    let mut stalled_sub =
        stalled_sub.unwrap_or_else(|| panic!("the stalled subscription was kept"));

    let started = Instant::now();
    let outcome = p
        .pump_once()
        .await
        .unwrap_or_else(|error| panic!("pump run 1: {error:?}"));
    assert_eq!(
        outcome.mapping.room_events, 130,
        "one page of 65 changes, two rooms each"
    );
    println!(
        "WP-PLT-08.T3: 130 publishes with one stalled of 1,000 subscribers: {:?}",
        started.elapsed()
    );

    // Batch 2 arrives once batch 1 is fully drained, so a healthy queue never holds more than
    // one batch (65 of 100 slots) however the scheduler behaves.
    let drain_deadline = Instant::now() + Duration::from_secs(60);
    while received.load(Ordering::SeqCst) < (SUBSCRIBERS - 1) * 65 {
        assert!(
            Instant::now() < drain_deadline,
            "the healthy subscribers did not drain batch 1 within 60s"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let started = Instant::now();
    for seq in 66..=130u64 {
        feed.push_change(
            seq,
            "sales_invoice",
            42,
            spot_storage_contract::ChangeOp::Insert,
            1,
        );
    }
    p.pump_once()
        .await
        .unwrap_or_else(|error| panic!("pump run 2: {error:?}"));
    println!(
        "WP-PLT-08.T3: another 130 publishes with the stall closed: {:?}",
        started.elapsed()
    );

    // Every healthy subscriber received both batches in full (130 events each).
    let mut healthy_total = 0;
    for task in tasks {
        healthy_total += task
            .await
            .unwrap_or_else(|error| panic!("a drain task panicked: {error:?}"));
    }
    assert_eq!(
        healthy_total,
        (SUBSCRIBERS - 1) * 130,
        "every healthy subscriber received both batches in full"
    );
    assert_eq!(
        closed.load(Ordering::SeqCst),
        0,
        "no healthy subscriber was ever closed"
    );

    // The stalled subscription reports exactly the documented close reason (wire 4008):
    // the close wins over its (discarded) buffered events.
    let closed = tokio::time::timeout(Duration::from_secs(5), stalled_sub.recv())
        .await
        .unwrap_or_else(|_| panic!("the close of the stalled subscriber must be reported"))
        .err()
        .unwrap_or_else(|| panic!("the stalled subscriber must be closed, not fed"));
    assert_eq!(
        closed,
        Closed::SlowConsumer,
        "the close reason the WebSocket transport turns into close code 4008"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pump_publish_run_is_bounded_while_one_subscriber_never_reads() {
    // The sharper proof for the pump: in one synchronous publish pass, the only queue
    // that can overflow is the stalled one — so the pass's bounded time and
    // "a stall delays nobody" hold structurally, not by scheduling luck. The two
    // subscribers share one 20-slot-capacity bus; the healthy one joins only the
    // doctype room (18 events, fits), the stalled one joins the doctype AND its
    // doc room (36 events, overflows mid-pass).
    let bus = bus_with(20, Arc::new(AllowAll));
    let site = site1();
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());

    let mut stuck = watcher(&bus, &site, "stuck");
    let mut healthy = watcher(&bus, &site, "healthy");
    let room = Room::doctype("sales_invoice").must();
    // The changes all touch the same document, stored-id 42: the pump maps its id to
    // the stored-id **text**, so the subscription must join exactly that doc room
    // (`doc:sales_invoice:<text of stored id 42>`). The stuck subscription follows the
    // doctype room AND that one document room, 18 + 18 = 36 events against a 20-slot
    // queue — it overflows mid-pass; the healthy subscriber sees only the 18 doctype
    // events, which always fit.
    let doc_room = Room::document("sales_invoice", &doc_id_text(42)).must();
    stuck.join(&room, None).await.must();
    healthy.join(&room, None).await.must();
    stuck.join(&doc_room, None).await.must();

    // The healthy reader drains concurrently with the pump pass.
    let closed = Arc::new(AtomicUsize::new(0));
    let healthy_task = tokio::spawn(drain_task(
        healthy,
        18,
        Arc::clone(&closed),
        Arc::new(AtomicUsize::new(0)),
    ));

    let p: FeedPump<FakeFeed, FakeTelemetry> =
        pump(&site, &feed, &bus, &store, &telemetry, 30, true);

    // 18 changes, all for stored id 42 → 18 doctype-room + 18 doc-room events (the
    // distinct seq stays on the feed; the room mapping folds every change onto one
    // document room). The stuck subscription receives both rooms (36 > 20 slots); the
    // healthy subscription only the doctype events (18 ≤ 20).
    for seq in 1..=18u64 {
        feed.push_change(
            seq,
            "sales_invoice",
            42,
            spot_storage_contract::ChangeOp::Update,
            1,
        );
    }
    let started = Instant::now();
    let outcome = p.pump_once().await.unwrap_or_else(|error| {
        panic!("the pump does not wait for a stalled subscriber: {error:?}")
    });
    let elapsed = started.elapsed();
    println!("WP-PLT-08.T3: 36 publishes with a stuck subscriber: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(10));
    assert_eq!(outcome.mapping.room_events, 36);

    // The stuck one is closed mid-pass; the healthy reader saw all 18, gap-free.
    let closed_stuck = tokio::time::timeout(Duration::from_secs(5), stuck.recv())
        .await
        .unwrap_or_else(|_| panic!("close must be reported"))
        .err()
        .unwrap_or_else(|| panic!("the stalled subscriber must be closed"));
    assert_eq!(closed_stuck, Closed::SlowConsumer);
    let seen = healthy_task
        .await
        .unwrap_or_else(|error| panic!("drain task panicked: {error:?}"));
    assert_eq!(
        seen, 18,
        "the healthy subscriber receives every event, gap-free"
    );
    assert_eq!(
        closed.load(Ordering::SeqCst),
        0,
        "no healthy subscriber was closed; the stall delayed no one"
    );
}

#[tokio::test]
async fn the_pump_lag_exposes_feed_position_versus_head_through_telemetry() {
    // The lag metric the host forwards to its metrics system: entries at or after the
    // stored position and at or before the head, per log. With a page limit of 3 over a
    // feed of 10, every run trails by what it has not read yet.
    let bus = bus_with(64, Arc::new(AllowAll));
    let site = site1();
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());
    let p: FeedPump<FakeFeed, FakeTelemetry> =
        pump(&site, &feed, &bus, &store, &telemetry, 3, true);

    for seq in 1..=10u64 {
        feed.push_change(
            seq,
            "sales_invoice",
            42 + seq as u32,
            spot_storage_contract::ChangeOp::Insert,
            1,
        );
    }
    feed.push_record(
        1,
        "identity_user_created",
        "user",
        9,
        None,
        &identity_payload(&user_text(9)),
    );
    feed.push_record(
        2,
        "doc_posted",
        "sales_invoice",
        42,
        Some(2),
        &empty_data_payload(),
    );
    feed.push_record(
        3,
        "doc_updated_posted",
        "sales_invoice",
        42,
        Some(3),
        &changed_fields_payload(&["total"]),
    );

    // Run 1: 3 of 10 changes read → lag 7; 3 of 3 outbox records read → lag 0.
    p.pump_once()
        .await
        .unwrap_or_else(|error| panic!("first run: {error:?}"));
    // The guard lives only inside this statement's scope — never across an `.await`.
    let (lag_changes, lag_outbox) = {
        let records = guard_telemetry(&telemetry);
        last_lags(&records)
    };
    assert_eq!(
        lag_changes,
        Some(7),
        "10 entries, 3 read: the pump trails by 7"
    );
    assert_eq!(lag_outbox, Some(0), "3 outbox entries, 3 published: no lag");

    // Runs 2–4 drain the change feed: pages of 3, 3, 1 — the last run trails by 0.
    let (lag_changes, _) = {
        for run in 2..=4 {
            p.pump_once()
                .await
                .unwrap_or_else(|error| panic!("run {run}: {error:?}"));
        }
        let records = guard_telemetry(&telemetry);
        last_lags(&records)
    };
    assert_eq!(lag_changes, Some(0), "drained: no lag");
}

fn guard_telemetry(
    telemetry: &Arc<FakeTelemetry>,
) -> std::sync::MutexGuard<'_, std::collections::VecDeque<TelemetryRecord>> {
    support::guard(&telemetry.records)
}

fn last_lags(records: &std::collections::VecDeque<TelemetryRecord>) -> (Option<u64>, Option<u64>) {
    let mut lag_changes = None;
    let mut lag_outbox = None;
    for record in records.iter().rev() {
        match record {
            TelemetryRecord::Lag("changes", behind) => {
                lag_changes = Some(*behind);
                if lag_outbox.is_some() {
                    break;
                }
            }
            TelemetryRecord::Lag("outbox", behind) => {
                lag_outbox = Some(*behind);
                if lag_changes.is_some() {
                    break;
                }
            }
            _ => {}
        }
    }
    (lag_changes, lag_outbox)
}

#[tokio::test]
async fn a_full_queue_with_no_head_wired_still_closes_only_the_stalled_one() {
    // The lag probe is optional (`head: None`); backpressure does not depend on it.
    let bus = bus_with(1, Arc::new(AllowAll));
    let site = site1();
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());
    let p: FeedPump<FakeFeed, FakeTelemetry> =
        pump(&site, &feed, &bus, &store, &telemetry, 100, false);

    let mut doomed = watcher(&bus, &site, "doomed");
    let mut other = watcher(&bus, &site, "other");
    let room = Room::doctype("sales_invoice").must();
    let other_room = Room::doctype("project").must();
    doomed.join(&room, None).await.must();
    other.join(&other_room, None).await.must();

    feed.push_change(
        1,
        "sales_invoice",
        42,
        spot_storage_contract::ChangeOp::Insert,
        1,
    );
    feed.push_change(
        2,
        "sales_invoice",
        43,
        spot_storage_contract::ChangeOp::Insert,
        1,
    );
    feed.push_change(1, "project", 44, spot_storage_contract::ChangeOp::Insert, 1);
    feed.push_record(
        1,
        "identity_user_created",
        "user",
        9,
        None,
        &identity_payload(&user_text(9)),
    );

    p.pump_once()
        .await
        .unwrap_or_else(|error| panic!("pump run: {error:?}"));

    let closed = tokio::time::timeout(Duration::from_secs(5), doomed.recv())
        .await
        .unwrap_or_else(|_| panic!("close must be reported"))
        .err()
        .unwrap_or_else(|| panic!("the stalled subscriber must be closed"));
    assert_eq!(closed, Closed::SlowConsumer);

    // A subscriber on a different room is untouched: `other`'s one-slot queue holds its
    // own room's event.
    let event = tokio::time::timeout(Duration::from_secs(5), other.recv())
        .await
        .unwrap_or_else(|_| panic!("recv timed out"))
        .unwrap_or_else(|closed_| panic!("subscription closed unexpectedly: {closed_:?}"));
    assert_eq!(event.room.as_str(), "doctype:project");
}
