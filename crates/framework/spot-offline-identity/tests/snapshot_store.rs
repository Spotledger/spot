//! Acceptance of snapshots and timestamps: rollback, fork, replay, expiry, signatures, identity.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_offline_identity::*;

fn fresh(world: &World) -> SnapshotStore<MemoryPersistence> {
    SnapshotStore::open(world.anchor(), MemoryPersistence::default(), clock(0, 1))
}

#[test]
fn accepts_a_valid_snapshot_and_starts_the_lease() {
    let w = World::new();
    let mut store = fresh(&w);
    let got = store.accept(&w.signed(snapshot(1)), rx(0, 1)).unwrap();
    assert_eq!(got.kind, AcceptedKind::Snapshot);
    assert_eq!(got.remaining_ms, [HOUR, 12 * HOUR, 24 * HOUR]);
    assert_eq!(store.highest_epoch(), 1);
    assert_eq!(store.lease_remaining_ms(Tier::One, clock(0, 1)), HOUR);
}

#[test]
fn a_lower_epoch_is_a_rollback_and_changes_nothing() {
    let w = World::new();
    let mut store = w.store_with(snapshot(5));
    let err = store.accept(&w.signed(snapshot(4)), rx(10, 1)).unwrap_err();
    assert_eq!(err, Rejection::Rollback { got: 4, held: 5 });
    assert_eq!(store.highest_epoch(), 5);
}

#[test]
fn an_equal_epoch_with_other_content_is_a_fork() {
    let w = World::new();
    let mut store = w.store_with(snapshot(5));
    let mut other = snapshot(5);
    other.directory[0].enabled = false;
    let err = store.accept(&w.signed(other), rx(10, 1)).unwrap_err();
    assert_eq!(err, Rejection::Fork { epoch: 5 });
    assert!(
        store.snapshot().unwrap().directory[0].enabled,
        "the held snapshot is untouched"
    );
}

#[test]
fn the_same_snapshot_again_is_a_replay_and_cannot_restore_a_spent_lease() {
    let w = World::new();
    let mut store = w.store_with(snapshot(5));
    let later = clock(2 * HOUR, 1);
    assert_eq!(store.lease_remaining_ms(Tier::One, later), 0);
    let err = store
        .accept(&w.signed(snapshot(5)), rx(2 * HOUR, 1))
        .unwrap_err();
    assert_eq!(err, Rejection::Replay { epoch: 5 });
    assert_eq!(store.lease_remaining_ms(Tier::One, later), 0);
}

#[test]
fn a_snapshot_whose_deadlines_have_passed_on_the_signed_hub_clock_is_expired() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    // The hub signs a time after every deadline of the next snapshot.
    store
        .accept(&w.hub_time(1, T0 + 25 * HOUR), rx(1_000, 1))
        .unwrap();
    let err = store
        .accept(&w.signed(snapshot(2)), rx(2_000, 1))
        .unwrap_err();
    assert_eq!(err, Rejection::Expired);
    assert_eq!(store.highest_epoch(), 1);
}

#[test]
fn a_partly_expired_snapshot_keeps_only_the_tiers_that_remain() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store
        .accept(&w.hub_time(1, T0 + 2 * HOUR), rx(1_000, 1))
        .unwrap();
    let got = store.accept(&w.signed(snapshot(2)), rx(2_000, 1)).unwrap();
    assert_eq!(got.remaining_ms, [0, 10 * HOUR, 22 * HOUR]);
}

#[test]
fn a_snapshot_lease_counts_from_its_signed_issue_time_whatever_the_channel_says() {
    // There is no input for "the hub's time as told by the transport": a first
    // snapshot gets exactly deadline minus its own signed issue time.
    let w = World::new();
    let mut store = fresh(&w);
    let got = store.accept(&w.signed(snapshot(1)), rx(0, 1)).unwrap();
    assert_eq!(got.remaining_ms, [HOUR, 12 * HOUR, 24 * HOUR]);
    assert_eq!(store.hub_time_ms(), T0);
}

#[test]
fn a_bad_signature_is_rejected() {
    let w = World::new();
    let mut store = fresh(&w);
    let Message::Snapshot(mut signed) = w.signed(snapshot(1)) else {
        panic!("snapshot")
    };
    signed.snapshot.directory[0].enabled = false; // altered after signing
    let err = store
        .accept(&Message::Snapshot(signed), rx(0, 1))
        .unwrap_err();
    assert_eq!(err, Rejection::BadSignature);
    assert_eq!(store.highest_epoch(), 0);
}

#[test]
fn a_snapshot_signed_by_a_key_the_root_never_delegated_is_rejected() {
    let w = World::new();
    let mut store = fresh(&w);
    let rogue = Key::new(9);
    let msg = Message::Snapshot(sign_snapshot(snapshot(1), &rogue, w.delegation.clone()));
    assert_eq!(
        store.accept(&msg, rx(0, 1)).unwrap_err(),
        Rejection::BadSignature
    );
}

#[test]
fn a_delegation_not_signed_by_the_root_is_rejected() {
    let w = World::new();
    let mut store = fresh(&w);
    let rogue = Key::new(9);
    let forged = sign_delegation(
        &rogue,
        &tenant(),
        1,
        &rogue.public_key(),
        &w.ts.public_key(),
    );
    let msg = Message::Snapshot(sign_snapshot(snapshot(1), &rogue, forged));
    assert_eq!(
        store.accept(&msg, rx(0, 1)).unwrap_err(),
        Rejection::BadDelegation
    );
}

#[test]
fn one_key_for_both_roles_is_refused() {
    let w = World::new();
    let mut store = fresh(&w);
    let same = sign_delegation(
        &w.root,
        &tenant(),
        1,
        &w.snap.public_key(),
        &w.snap.public_key(),
    );
    let msg = Message::Snapshot(sign_snapshot(snapshot(1), &w.snap, same));
    assert_eq!(
        store.accept(&msg, rx(0, 1)).unwrap_err(),
        Rejection::BadDelegation
    );
}

#[test]
fn an_older_delegation_cannot_come_back() {
    let w = World::new();
    let v2 = sign_delegation(
        &w.root,
        &tenant(),
        2,
        &w.snap.public_key(),
        &w.ts.public_key(),
    );
    let mut store = fresh(&w);
    let msg = Message::Snapshot(sign_snapshot(snapshot(1), &w.snap, v2));
    store.accept(&msg, rx(0, 1)).unwrap();
    let old = w.signed(snapshot(2)); // delegation version 1
    assert_eq!(
        store.accept(&old, rx(1, 1)).unwrap_err(),
        Rejection::DelegationRollback { got: 1, held: 2 }
    );
}

#[test]
fn a_snapshot_for_another_node_or_tenant_is_rejected() {
    let w = World::new();
    let mut store = fresh(&w);
    let mut other_node = snapshot(1);
    other_node.node = NodeId::new("branch-2").unwrap();
    assert_eq!(
        store.accept(&w.signed(other_node), rx(0, 1)).unwrap_err(),
        Rejection::WrongNode
    );
    let mut other_tenant = snapshot(1);
    other_tenant.tenant = TenantId::new("globex").unwrap();
    assert_eq!(
        store.accept(&w.signed(other_tenant), rx(0, 1)).unwrap_err(),
        Rejection::WrongTenant
    );
}

#[test]
fn a_delegation_for_another_tenant_is_rejected() {
    let w = World::new();
    let mut store = fresh(&w);
    let other = sign_delegation(
        &w.root,
        &TenantId::new("globex").unwrap(),
        1,
        &w.snap.public_key(),
        &w.ts.public_key(),
    );
    let msg = Message::Snapshot(sign_snapshot(snapshot(1), &w.snap, other));
    assert_eq!(
        store.accept(&msg, rx(0, 1)).unwrap_err(),
        Rejection::WrongTenant
    );
}

#[test]
fn leases_outside_the_floor_and_ceiling_are_rejected() {
    let w = World::new();
    let mut store = fresh(&w);
    let short = snapshot_at(1, T0, [59_999, HOUR, HOUR]);
    assert_eq!(
        store.accept(&w.signed(short), rx(0, 1)).unwrap_err(),
        Rejection::BoundsOutOfRange { tier: Tier::One }
    );
    let long = snapshot_at(1, T0, [HOUR, HOUR, CEILING_MS + 1]);
    assert_eq!(
        store.accept(&w.signed(long), rx(0, 1)).unwrap_err(),
        Rejection::BoundsOutOfRange { tier: Tier::Three }
    );
    let edge = snapshot_at(1, T0, [FLOOR_MS, HOUR, CEILING_MS]);
    store.accept(&w.signed(edge), rx(0, 1)).unwrap();
}

#[test]
fn a_duplicate_directory_entry_is_malformed() {
    let w = World::new();
    let mut store = fresh(&w);
    let mut dup = snapshot(1);
    let mut twin = dup.directory[0].clone();
    twin.enabled = false;
    dup.directory.push(twin);
    assert!(matches!(
        store.accept(&w.signed(dup), rx(0, 1)).unwrap_err(),
        Rejection::Malformed { .. }
    ));
}

#[test]
fn list_order_does_not_change_the_hash() {
    let a = snapshot(1);
    let mut b = snapshot(1);
    b.directory.reverse();
    b.grants.reverse();
    assert_eq!(a.hash(), b.hash());
    let mut c = snapshot(1);
    c.directory[0].enabled = false;
    assert_ne!(a.hash(), c.hash());
}

#[test]
fn a_timestamp_extends_the_lease_without_changing_the_snapshot() {
    let w = World::new();
    let snap = snapshot(1);
    let mut store = w.store_with(snap.clone());
    let at = T0 + 30 * 60_000;
    let msg = w.timestamp(&snap, at, [HOUR, 12 * HOUR, 24 * HOUR]);
    let got = store.accept(&msg, rx(30 * 60_000, 1)).unwrap();
    assert_eq!(got.kind, AcceptedKind::Timestamp);
    assert_eq!(got.remaining_ms, [HOUR, 12 * HOUR, 24 * HOUR]);
    assert_eq!(store.snapshot().unwrap().hash(), snap.hash());
    assert_eq!(store.highest_epoch(), 1);
    assert_eq!(
        store.lease_remaining_ms(Tier::One, clock(30 * 60_000, 1)),
        HOUR
    );
}

#[test]
fn a_stale_or_replayed_timestamp_is_rejected() {
    let w = World::new();
    let snap = snapshot(1);
    let mut store = w.store_with(snap.clone());
    let first = w.timestamp(&snap, T0 + 1_000, [HOUR, HOUR, HOUR]);
    store.accept(&first, rx(1_000, 1)).unwrap();
    assert_eq!(
        store.accept(&first, rx(2_000, 1)).unwrap_err(),
        Rejection::StaleTimestamp
    );
    let older = w.timestamp(&snap, T0 + 500, [24 * HOUR; 3]);
    assert_eq!(
        store.accept(&older, rx(2_000, 1)).unwrap_err(),
        Rejection::StaleTimestamp
    );
}

#[test]
fn a_timestamp_for_another_snapshot_or_with_none_held_is_rejected() {
    let w = World::new();
    let snap = snapshot(1);
    let mut empty = fresh(&w);
    let msg = w.timestamp(&snap, T0 + 1, [HOUR; 3]);
    assert_eq!(
        empty.accept(&msg, rx(0, 1)).unwrap_err(),
        Rejection::TimestampMismatch
    );
    let mut store = w.store_with(snap);
    let mut forked = snapshot(1);
    forked.key_epoch = 99;
    let msg = w.timestamp(&forked, T0 + 1, [HOUR; 3]);
    assert_eq!(
        store.accept(&msg, rx(0, 1)).unwrap_err(),
        Rejection::TimestampMismatch
    );
    let msg = w.timestamp(&snapshot(2), T0 + 1, [HOUR; 3]);
    assert_eq!(
        store.accept(&msg, rx(0, 1)).unwrap_err(),
        Rejection::TimestampMismatch
    );
}

#[test]
fn a_timestamp_signed_with_the_snapshot_key_is_rejected() {
    let w = World::new();
    let snap = snapshot(1);
    let mut store = w.store_with(snap.clone());
    let ts = Timestamp {
        tenant: tenant(),
        node: node(),
        epoch: 1,
        snapshot_hash: snap.hash(),
        signed_at: T0 + 1,
        valid_until: TierDeadlines([T0 + 1 + HOUR; 3]),
        nonce: T0 + 1,
    };
    let msg = Message::Timestamp(sign_timestamp(ts, &w.snap, w.delegation.clone()));
    assert_eq!(
        store.accept(&msg, rx(0, 1)).unwrap_err(),
        Rejection::BadSignature
    );
}

#[test]
fn a_failed_save_accepts_nothing() {
    let w = World::new();
    let mut store = SnapshotStore::open(
        w.anchor(),
        MemoryPersistence {
            state: None,
            fail_saves: true,
            seen_nonce: 0,
        },
        clock(0, 1),
    );
    assert_eq!(
        store.accept(&w.signed(snapshot(1)), rx(0, 1)).unwrap_err(),
        Rejection::PersistFailed
    );
    assert_eq!(store.highest_epoch(), 0);
    assert!(store.snapshot().is_none());
}
