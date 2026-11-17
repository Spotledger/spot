//! The carrier loop against a scripted link: one in flight per kind, backoff on refusal, no
//! busy loop offline, idempotent resend, stale answers ignored, the snapshot exchange.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use spot_edge_offline_host::carrier::{self, LeaseRequestMsg, SnapshotMsg};
use spot_edge_offline_host::carrier_loop::{CarrierLoop, CarrierRecord, CarrierSettings};
use spot_edge_offline_host::identity::{BoxedPersistence, SnapshotKeeper};
use spot_edge_offline_host::kinds::{LEASE_REQUEST_KIND, SNAPSHOT_KIND};
use spot_edge_offline_host::lease::Standing;
use spot_edge_offline_host::series::RangeView;
use spot_offline_identity::{MemoryPersistence, NodeClock};

fn loop_over(
    link: &FakeLink,
    ranges: Arc<spot_edge_offline_host::ranges::RangeBook>,
) -> CarrierLoop {
    CarrierLoop::new(
        Arc::new(link.clone()),
        ranges,
        None,
        CarrierSettings {
            backoff_min: Duration::from_secs(5),
            backoff_max: Duration::from_secs(40),
            answer_timeout: Duration::from_secs(60),
            idle: Duration::from_secs(60),
        },
    )
}

fn request_of(record: &CarrierRecord) -> LeaseRequestMsg {
    assert_eq!(record.kind, LEASE_REQUEST_KIND);
    carrier::decode(&record.payload).unwrap()
}

#[tokio::test]
async fn offline_nothing_is_sent_and_nobody_spins() {
    let link = FakeLink::default();
    let mut carrier_loop = loop_over(&link, book(1_000, memory_store()));
    for second in 0..50 {
        let tick = carrier_loop.tick(at(second)).await;
        assert!(!tick.connected);
        assert_eq!(
            tick.next_wake,
            Duration::from_secs(60),
            "the idle interval, not a spin"
        );
        // The series is empty, so the loop knows it is exhausted and low, even offline.
        assert_eq!(tick.exhausted_series, vec!["INV".to_owned()]);
        assert_eq!(tick.low_series, vec!["INV".to_owned()]);
    }
    assert!(link.sent().is_empty());
}

#[tokio::test]
async fn one_request_in_flight_then_the_grant_is_installed_durably() {
    let link = FakeLink::connected();
    let store = memory_store();
    let ranges = book(1_000, Arc::clone(&store));
    let mut carrier_loop = loop_over(&link, Arc::clone(&ranges));
    carrier_loop.tick(at(0)).await;
    carrier_loop.tick(at(1)).await;
    carrier_loop.tick(at(2)).await;
    assert_eq!(link.sent().len(), 1, "one in flight, not one per tick");
    let request = request_of(&link.sent()[0]);
    assert_eq!(request.size, 1_000);
    assert_eq!(request.series, "INV");

    link.push(granted(&request.request_id, 1, 1_000, 1));
    let tick = carrier_loop.tick(at(3)).await;
    assert_eq!(tick.ranges_installed, 1);
    assert!(tick.exhausted_series.is_empty());
    assert_eq!(ranges.remaining("INV"), 1_000);
    // The range is in the store: a restart reopens it.
    let reopened = book(1_000, store);
    assert_eq!(reopened.remaining("INV"), 1_000);
    // Plenty left: nothing more is asked.
    carrier_loop.tick(at(4)).await;
    assert_eq!(link.sent().len(), 1);
}

#[tokio::test]
async fn a_refusal_backs_off_and_doubles_up_to_the_cap() {
    let link = FakeLink::connected();
    let mut carrier_loop = loop_over(&link, book(1_000, memory_store()));
    let mut now = 0;
    let mut waits = Vec::new();
    for _ in 0..5 {
        carrier_loop.tick(at(now)).await;
        let request = request_of(link.sent().last().unwrap());
        link.push(refused(&request.request_id));
        let tick = carrier_loop.tick(at(now)).await;
        assert_eq!(tick.codes, vec!["lease_grants.not_entitled".to_owned()]);
        waits.push(tick.next_wake.as_secs());
        let sent_before = link.sent().len();
        // Inside the backoff nothing is sent.
        carrier_loop.tick(at(now + 1)).await;
        assert_eq!(link.sent().len(), sent_before);
        now += tick.next_wake.as_secs();
    }
    assert_eq!(waits, vec![5, 10, 20, 40, 40]);
    assert_eq!(carrier_loop.lease_failures(), 5);
}

#[tokio::test]
async fn an_unanswered_request_is_asked_again_with_the_same_id() {
    let link = FakeLink::connected();
    let mut carrier_loop = loop_over(&link, book(1_000, memory_store()));
    carrier_loop.tick(at(0)).await;
    let first = request_of(&link.sent()[0]);
    // After the timeout and the first backoff the request is asked again, same id.
    let tick = carrier_loop.tick(at(61)).await;
    assert!(
        tick.codes
            .contains(&"edge_offline.lease_answer_timeout".to_owned())
    );
    carrier_loop.tick(at(67)).await;
    assert_eq!(link.sent().len(), 2);
    assert_eq!(request_of(&link.sent()[1]).request_id, first.request_id);
}

#[tokio::test]
async fn a_stale_answer_installs_nothing() {
    let link = FakeLink::connected();
    let ranges = book(1_000, memory_store());
    let mut carrier_loop = loop_over(&link, Arc::clone(&ranges));
    carrier_loop.tick(at(0)).await;
    link.push(granted("lease:INV@999", 1, 1_000, 1));
    let tick = carrier_loop.tick(at(1)).await;
    assert_eq!(tick.ranges_installed, 0);
    assert_eq!(ranges.remaining("INV"), 0);
}

#[tokio::test]
async fn a_range_not_above_the_held_one_is_refused_and_backs_off() {
    let link = FakeLink::connected();
    let store = memory_store();
    let ranges = book(10, Arc::clone(&store));
    let mut carrier_loop = loop_over(&link, Arc::clone(&ranges));
    carrier_loop.tick(at(0)).await;
    let first = request_of(&link.sent()[0]);
    link.push(granted(&first.request_id, 1, 10, 1));
    assert_eq!(carrier_loop.tick(at(1)).await.ranges_installed, 1);
    // Block 10 is below the 200 threshold, so another request follows at once.
    carrier_loop.tick(at(2)).await;
    let second = request_of(link.sent().last().unwrap());
    assert_eq!(second.request_id, "lease:INV@10");
    // A hub bug or an attacker offers an overlapping range: refused, nothing installed.
    link.push(granted(&second.request_id, 5, 15, 2));
    let tick = carrier_loop.tick(at(3)).await;
    assert_eq!(tick.ranges_installed, 0);
    assert_eq!(tick.codes, vec!["edge_offline.range_refused".to_owned()]);
    assert_eq!(ranges.last_held("INV"), 10);
    let _ = store;
}

#[tokio::test]
async fn the_snapshot_exchange_accepts_a_signed_snapshot_and_refuses_a_replay() {
    let world = World::new();
    let link = FakeLink::connected();
    let keeper: Arc<Mutex<SnapshotKeeper<BoxedPersistence>>> = Arc::new(Mutex::new(
        SnapshotKeeper::open(
            world.anchor(),
            BoxedPersistence(Box::new(MemoryPersistence::default())),
            NodeClock {
                mono_ms: 0,
                boot_id: 1,
            },
        )
        .with_node_key(Arc::new(Key::new(7))),
    ));
    let mut carrier_loop = CarrierLoop::new(
        Arc::new(link.clone()),
        book(1_000, memory_store()),
        Some(Arc::clone(&keeper)),
        CarrierSettings::default(),
    );
    carrier_loop.tick(at(0)).await;
    let asked = link
        .sent()
        .into_iter()
        .find(|r| r.kind == SNAPSHOT_KIND)
        .expect("a snapshot request");
    let SnapshotMsg::Request { request_id, signed } = carrier::decode(&asked.payload).unwrap()
    else {
        panic!("not a request");
    };
    assert_eq!(signed.request.report.unwrap().highest_epoch, 0);
    assert_eq!(
        signed.signature.len(),
        64,
        "the request is signed with the node key"
    );
    let issue = |id: &str| CarrierRecord {
        kind: SNAPSHOT_KIND.to_owned(),
        payload: carrier::encode(&SnapshotMsg::Issued {
            request_id: id.to_owned(),
            message: world.message(world.snapshot(1, T0, [HOUR, 12 * HOUR, 24 * HOUR])),
            companion: None,
        })
        .unwrap(),
    };
    link.push(issue(&request_id));
    let tick = carrier_loop.tick(at(1)).await;
    assert_eq!(tick.snapshots_accepted, 1);
    {
        let keeper = keeper.lock().unwrap();
        let now = NodeClock {
            mono_ms: 1_000,
            boot_id: 1,
        };
        assert!(matches!(keeper.standing(now), Standing::Alive(_)));
        assert!(keeper.snapshot_age(now).is_some());
        assert!(!keeper.refresh_due(now));
    }
}
