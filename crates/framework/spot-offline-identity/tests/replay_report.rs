//! A replayed disk image: the node reports what it holds, and a replayed lease image restores
//! no time (the node half; the hub half compares the report with what it issued).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_offline_identity::signing::{
    NodeReport, RequestKind, RequestRefusal, sign_node_request, verify_node_request,
};
use spot_offline_identity::*;

fn reopen(w: &World, p: MemoryPersistence, now: NodeClock) -> SnapshotStore<MemoryPersistence> {
    SnapshotStore::open(w.anchor(), p, now)
}

#[test]
fn a_node_restored_from_an_older_image_reports_the_older_epoch() {
    let w = World::new();
    let node_key = Key::new(9);
    let store = w.store_with(snapshot(1));
    // The epoch-1 copy of the disk (state and fence both).
    let epoch_one_image = store.into_persistence();
    let mut store = reopen(&w, epoch_one_image.clone(), clock(5, 2));
    store
        .accept(&w.signed(snapshot_at(2, T0 + 1_000, [HOUR; 3])), rx(10, 2))
        .unwrap();
    assert_eq!(store.report().highest_epoch, 2);
    let honest = sign_node_request(store.node_request(RequestKind::HubTime), &node_key);
    assert_eq!(
        verify_node_request(&node_key.public_key(), &honest)
            .unwrap()
            .highest_epoch,
        2
    );

    // An attacker puts the whole epoch-1 image back while the node is off.
    let replayed = reopen(&w, epoch_one_image, clock(0, 3));
    let signed = sign_node_request(replayed.node_request(RequestKind::HubTime), &node_key);
    let report = verify_node_request(&node_key.public_key(), &signed).unwrap();
    assert_eq!(report.highest_epoch, 1);
    assert_eq!(report.snapshot_hash, Some(snapshot(1).hash()));
    assert_eq!(report.last_attested, T0);
    assert_eq!(*report, replayed.report());
}

#[test]
fn the_report_cannot_be_altered_and_a_request_needs_one() {
    let w = World::new();
    let node_key = Key::new(9);
    let store = w.store_with(snapshot(3));
    let signed = sign_node_request(store.node_request(RequestKind::Snapshot), &node_key);
    assert!(verify_node_request(&node_key.public_key(), &signed).is_ok());

    // Every field of the report is covered by the signature.
    let alterations: [fn(&mut NodeReport); 4] = [
        |r| r.highest_epoch += 1,
        |r| r.snapshot_hash = None,
        |r| r.last_attested += 1,
        |r| r.hub_nonce += 1,
    ];
    for alter in alterations {
        let mut forged = signed.clone();
        alter(forged.request.report.as_mut().unwrap());
        assert_eq!(
            verify_node_request(&node_key.public_key(), &forged).unwrap_err(),
            RequestRefusal::BadSignature
        );
    }
    // So is the kind, and another key does not verify.
    let mut kind = signed.clone();
    kind.request.kind = RequestKind::HubTime;
    assert!(verify_node_request(&node_key.public_key(), &kind).is_err());
    assert!(verify_node_request(&Key::new(8).public_key(), &signed).is_err());

    // Stripping the report fails the signature; signing a request without one is still refused.
    let mut stripped = signed.clone();
    stripped.request.report = None;
    assert_eq!(
        verify_node_request(&node_key.public_key(), &stripped).unwrap_err(),
        RequestRefusal::BadSignature
    );
    let mut bare = store.node_request(RequestKind::Snapshot);
    bare.report = None;
    let bare = sign_node_request(bare, &node_key);
    assert_eq!(
        verify_node_request(&node_key.public_key(), &bare).unwrap_err(),
        RequestRefusal::ReportMissing
    );
}

#[test]
fn a_lease_image_older_than_the_newest_nonce_restores_nothing_until_the_hub_answers() {
    let w = World::new();
    let snap = snapshot(1);
    let mut store = w.store_with(snap.clone());
    // Hub contact one: nonce 1, lease one hour.
    store
        .accept(
            &w.timestamp_with_nonce(&snap, T0 + 10, [HOUR; 3], 1),
            rx(10_000, 1),
        )
        .unwrap();
    // The disk image taken right after that contact.
    store.checkpoint(clock(10_000, 1)).unwrap();
    let persistence = store.into_persistence();
    let image = persistence.state.clone().unwrap();
    let mut store = reopen(&w, persistence, clock(10_000, 1));
    // Hub contact two: nonce 2. The fence now knows nonce 2.
    store
        .accept(
            &w.timestamp_with_nonce(&snap, T0 + 20, [HOUR; 3], 2),
            rx(20_000, 1),
        )
        .unwrap();
    // The hub then goes away and the lease runs down; the node saves.
    let down = clock(2 * HOUR, 1);
    store.checkpoint(down).unwrap();
    let mut disk = store.into_persistence();
    assert_eq!(disk.nonce_floor(), 2);
    let honest = reopen(&w, disk.clone(), clock(0, 2));
    assert_eq!(honest.lease_remaining_ms(Tier::One, clock(0, 2)), 0);

    // On another boot the attacker puts the old image back, but not the fence.
    disk.state = Some(image);
    let mut replayed = reopen(&w, disk, clock(0, 3));
    assert_eq!(replayed.hub_nonce(), 1, "the image holds the older nonce");
    for tier in Tier::ALL {
        assert_eq!(replayed.lease_remaining_ms(tier, clock(0, 3)), 0);
    }
    // Only a new hub reply (a nonce newer than the one it holds) brings time back.
    replayed
        .accept(
            &w.timestamp_with_nonce(&snap, T0 + 30, [HOUR; 3], 3),
            rx(1_000, 3),
        )
        .unwrap();
    assert_eq!(
        replayed.lease_remaining_ms(Tier::One, clock(1_000, 3)),
        HOUR
    );
}

#[test]
fn an_image_at_the_newest_nonce_keeps_the_ordinary_thirty_second_charge() {
    let w = World::new();
    let snap = snapshot(1);
    let mut store = w.store_with(snap.clone());
    store
        .accept(
            &w.timestamp_with_nonce(&snap, T0 + 10, [HOUR; 3], 5),
            rx(0, 1),
        )
        .unwrap();
    store.checkpoint(clock(60_000, 1)).unwrap();
    let rebooted = reopen(&w, store.into_persistence(), clock(0, 2));
    assert_eq!(
        rebooted.lease_remaining_ms(Tier::One, clock(0, 2)),
        HOUR - 60_000 - MAX_CHECKPOINT_GAP
    );
}

#[test]
fn a_reply_whose_nonce_does_not_advance_is_stale() {
    let w = World::new();
    let snap = snapshot(1);
    let mut store = w.store_with(snap.clone());
    store
        .accept(
            &w.timestamp_with_nonce(&snap, T0 + 10, [HOUR; 3], 7),
            rx(0, 1),
        )
        .unwrap();
    for nonce in [7, 6, 0] {
        let again = w.timestamp_with_nonce(&snap, T0 + 20, [HOUR; 3], nonce);
        assert_eq!(
            store.accept(&again, rx(1, 1)).unwrap_err(),
            Rejection::StaleTimestamp
        );
        let time = w.hub_time_full(&w.ts, 1, T0 + 20, nonce);
        assert_eq!(
            store.accept(&time, rx(1, 1)).unwrap_err(),
            Rejection::StaleTimestamp
        );
    }
    assert_eq!(store.hub_nonce(), 7);
}
