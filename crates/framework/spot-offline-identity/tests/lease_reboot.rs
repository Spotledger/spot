//! The lease across restarts and clock trouble: a reboot can never extend it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_offline_identity::*;

fn reopen(w: &World, p: MemoryPersistence, now: NodeClock) -> SnapshotStore<MemoryPersistence> {
    SnapshotStore::open(w.anchor(), p, now)
}

#[test]
fn a_reboot_restores_at_most_the_saved_remaining_time() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    // 20 minutes pass, the node saves, then it reboots.
    store.checkpoint(clock(20 * 60_000, 1)).unwrap();
    let rebooted = reopen(&w, store.into_persistence(), clock(0, 2));
    assert_eq!(
        rebooted.lease_remaining_ms(Tier::One, clock(0, 2)),
        40 * 60_000 - MAX_CHECKPOINT_GAP
    );
    assert_eq!(
        rebooted.lease_remaining_ms(Tier::Three, clock(0, 2)),
        24 * HOUR - 20 * 60_000 - MAX_CHECKPOINT_GAP
    );
}

#[test]
fn a_reboot_with_the_monotonic_clock_at_a_larger_or_smaller_value_gains_nothing() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(10 * 60_000, 1)).unwrap();
    let saved = store.into_persistence();
    for start in [0, 1, 10 * 60_000, 99 * HOUR] {
        let r = reopen(&w, saved.clone(), clock(start, 2));
        assert!(r.lease_remaining_ms(Tier::One, clock(start, 2)) <= 50 * 60_000);
        // And it counts down from there on the new boot.
        let later = clock(start + 50 * 60_000, 2);
        assert_eq!(r.lease_remaining_ms(Tier::One, later), 0);
    }
}

#[test]
fn many_reboots_in_a_row_never_grow_the_lease() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(5 * 60_000, 1)).unwrap();
    let mut p = store.into_persistence();
    let mut last = 55 * 60_000;
    for boot in 2..8 {
        let mut s = reopen(&w, p, clock(0, boot));
        let now = s.lease_remaining_ms(Tier::One, clock(0, boot));
        assert!(now <= last);
        last = now;
        s.checkpoint(clock(0, boot)).unwrap();
        p = s.into_persistence();
    }
}

#[test]
fn a_process_restart_on_the_same_boot_subtracts_the_time_that_passed() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(10 * 60_000, 1)).unwrap();
    let restarted = reopen(&w, store.into_persistence(), clock(40 * 60_000, 1));
    assert_eq!(
        restarted.lease_remaining_ms(Tier::One, clock(40 * 60_000, 1)),
        20 * 60_000
    );
}

#[test]
fn a_restart_on_the_same_boot_with_a_clock_that_went_backwards_reads_as_lapsed() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(30 * 60_000, 1)).unwrap();
    let restarted = reopen(&w, store.into_persistence(), clock(5 * 60_000, 1));
    for t in Tier::ALL {
        assert_eq!(restarted.lease_remaining_ms(t, clock(5 * 60_000, 1)), 0);
    }
}

#[test]
fn a_clock_that_goes_backwards_while_running_fails_closed() {
    let w = World::new();
    let mut store = SnapshotStore::open(w.anchor(), MemoryPersistence::default(), clock(0, 1));
    store.accept(&w.signed(snapshot(1)), rx(10_000, 1)).unwrap();
    store
        .renew_certificate(1, HOUR, true, clock(10_000, 1))
        .unwrap();
    assert!(store.sign_in("clerk", clock(20_000, 1)).is_allowed());
    assert_eq!(store.lease_remaining_ms(Tier::Three, clock(5_000, 1)), 0);
    assert_eq!(
        store.sign_in("clerk", clock(5_000, 1)),
        Decision::Deny(DenyReason::ClockFault)
    );
}

#[test]
fn a_boot_id_that_changes_without_a_restore_fails_closed() {
    let w = World::new();
    let store = w.store_with(snapshot(1));
    assert_eq!(store.lease_remaining_ms(Tier::Three, clock(1_000, 2)), 0);
    assert_eq!(
        store.authorise_offline("clerk", Tier::Three, clock(1_000, 2)),
        Decision::Deny(DenyReason::ClockFault)
    );
}

#[test]
fn the_wall_clock_is_not_an_input() {
    // Nothing in the API takes a wall-clock reading for lease arithmetic: the same monotonic
    // readings give the same answers whatever the wall clock says. This pins the shape of
    // the inputs: a `NodeClock` (monotonic and boot id) and the hub's authenticated time.
    let w = World::new();
    let a = w.store_with(snapshot(1));
    let b = w.store_with(snapshot(1));
    for mono in [0, 1_000, HOUR - 1, HOUR, 13 * HOUR] {
        assert_eq!(
            a.lease_remaining_ms(Tier::One, clock(mono, 1)),
            b.lease_remaining_ms(Tier::One, clock(mono, 1))
        );
    }
}

#[test]
fn a_restart_keeps_the_epoch_counters_so_an_old_snapshot_cannot_return() {
    let w = World::new();
    let store = w.store_with(snapshot(7));
    let mut r = reopen(&w, store.into_persistence(), clock(0, 2));
    assert_eq!(r.highest_epoch(), 7);
    assert_eq!(
        r.accept(&w.signed(snapshot(6)), rx(0, 2)).unwrap_err(),
        Rejection::Rollback { got: 6, held: 7 }
    );
    assert!(matches!(
        r.accept(&w.signed(snapshot(7)), rx(0, 2)).unwrap_err(),
        Rejection::Replay { .. }
    ));
}

#[test]
fn a_damaged_saved_snapshot_fails_closed_but_keeps_the_rollback_defence() {
    let w = World::new();
    let store = w.store_with(snapshot(7));
    let mut saved = store.into_persistence();
    if let Some(state) = saved.state.as_mut()
        && let Some(signed) = state.signed.as_mut()
    {
        signed.snapshot.directory[0].enabled = false; // altered on disk
    }
    let mut r = reopen(&w, saved, clock(0, 2));
    assert!(r.recovered_from_damage());
    assert!(r.snapshot().is_none());
    r.renew_certificate(5, HOUR, true, clock(0, 2)).unwrap();
    assert_eq!(
        r.authorise_offline("clerk", Tier::Three, clock(0, 2)),
        Decision::Deny(DenyReason::NoSnapshot)
    );
    assert_eq!(
        r.accept(&w.signed(snapshot(3)), rx(0, 2)).unwrap_err(),
        Rejection::Rollback { got: 3, held: 7 }
    );
    // A genuine newer snapshot recovers the node.
    r.accept(&w.signed(snapshot(8)), rx(0, 2)).unwrap();
    assert!(!r.recovered_from_damage());
}

#[test]
fn a_timestamp_after_a_reboot_extends_from_the_new_boot() {
    let w = World::new();
    let snap = snapshot(1);
    let mut store = w.store_with(snap.clone());
    store.checkpoint(clock(55 * 60_000, 1)).unwrap();
    let mut r = reopen(&w, store.into_persistence(), clock(0, 2));
    assert_eq!(
        r.lease_remaining_ms(Tier::One, clock(0, 2)),
        5 * 60_000 - MAX_CHECKPOINT_GAP
    );
    let at = T0 + 56 * 60_000;
    r.accept(
        &w.timestamp(&snap, at, [HOUR, 12 * HOUR, 24 * HOUR]),
        rx(1_000, 2),
    )
    .unwrap();
    assert_eq!(r.lease_remaining_ms(Tier::One, clock(1_000, 2)), HOUR);
}

// ---- Recorded security findings: signed time only, and the unmeasured gap is charged ----

fn all_remaining(s: &SnapshotStore<MemoryPersistence>, now: NodeClock) -> [u64; 3] {
    Tier::ALL.map(|t| s.lease_remaining_ms(t, now))
}

#[test]
fn a_restart_on_another_boot_charges_the_checkpoint_gap_against_every_tier() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(10 * 60_000, 1)).unwrap();
    let saved = all_remaining(&store, clock(10 * 60_000, 1));
    let rebooted = reopen(&w, store.into_persistence(), clock(0, 2));
    let after = all_remaining(&rebooted, clock(0, 2));
    for t in Tier::ALL {
        let i = t.index();
        assert_eq!(after[i], saved[i] - MAX_CHECKPOINT_GAP);
        assert!(after[i] < saved[i]);
    }
}

#[test]
fn a_long_power_off_does_not_extend_the_lease() {
    // Whatever the new boot's monotonic clock starts at, the remaining lease is never more
    // than what was saved less the charge.
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(30 * 60_000, 1)).unwrap();
    let saved = all_remaining(&store, clock(30 * 60_000, 1));
    let p = store.into_persistence();
    for start in [0, 5 * 60_000, 9 * HOUR] {
        let r = reopen(&w, p.clone(), clock(start, 2));
        let after = all_remaining(&r, clock(start, 2));
        for t in Tier::ALL {
            assert!(after[t.index()] <= saved[t.index()].saturating_sub(MAX_CHECKPOINT_GAP));
        }
    }
}

#[test]
fn a_restart_on_the_same_boot_is_measured_and_not_charged() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(10 * 60_000, 1)).unwrap();
    let r = reopen(&w, store.into_persistence(), clock(10 * 60_000 + 5_000, 1));
    assert_eq!(
        r.lease_remaining_ms(Tier::One, clock(10 * 60_000 + 5_000, 1)),
        50 * 60_000 - 5_000
    );
}

#[test]
fn a_signed_hub_time_restores_the_exact_lease_after_a_restart() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(10 * 60_000, 1)).unwrap();
    let mut r = reopen(&w, store.into_persistence(), clock(0, 2));
    let charged = r.lease_remaining_ms(Tier::One, clock(0, 2));
    assert_eq!(charged, 50 * 60_000 - MAX_CHECKPOINT_GAP);
    // The hub says it is 10 min 20 s after issue: exact remaining is 49 min 40 s.
    let now = clock(1_000, 2);
    let got = r
        .accept(&w.hub_time(1, T0 + 10 * 60_000 + 20_000), rx(1_000, 2))
        .unwrap();
    assert_eq!(got.kind, AcceptedKind::HubTime);
    let exact = 50 * 60_000 - 20_000;
    assert_eq!(got.remaining_ms[0], exact);
    assert!(exact > charged);
    assert_eq!(r.lease_remaining_ms(Tier::One, now), exact);
    assert_eq!(r.hub_time_ms(), T0 + 10 * 60_000 + 20_000);
}

#[test]
fn a_signed_hub_time_never_restores_above_what_was_saved() {
    // The hub time is early (a delayed capture): the exact value would be above what the node
    // saved, so it is capped at what was saved.
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store.checkpoint(clock(30 * 60_000, 1)).unwrap();
    let mut r = reopen(&w, store.into_persistence(), clock(0, 2));
    let got = r.accept(&w.hub_time(1, T0 + 1), rx(0, 2)).unwrap();
    assert_eq!(got.remaining_ms[0], 30 * 60_000);
    assert_eq!(r.lease_remaining_ms(Tier::One, clock(0, 2)), 30 * 60_000);
}

#[test]
fn a_signed_hub_time_never_extends_a_lease_that_was_not_charged() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    let before = store.lease_remaining_ms(Tier::One, clock(20 * 60_000, 1));
    store
        .accept(&w.hub_time(1, T0 + 1_000), rx(20 * 60_000, 1))
        .unwrap();
    assert!(store.lease_remaining_ms(Tier::One, clock(20 * 60_000, 1)) <= before);
}

#[test]
fn a_hub_time_past_every_deadline_lapses_the_lease() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    let got = store
        .accept(&w.hub_time(1, T0 + 25 * HOUR), rx(1_000, 1))
        .unwrap();
    assert_eq!(got.remaining_ms, [0, 0, 0]);
    assert_eq!(
        store.authorise_offline("clerk", Tier::Three, clock(1_000, 1)),
        Decision::Deny(DenyReason::TierLapsed(Tier::Three))
    );
}

#[test]
fn a_signed_time_is_checkpointed_so_a_restart_keeps_the_clock() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store
        .accept(&w.hub_time(1, T0 + 5 * 60_000), rx(1_000, 1))
        .unwrap();
    let r = reopen(&w, store.into_persistence(), clock(0, 2));
    assert_eq!(r.hub_time_ms(), T0 + 5 * 60_000);
}

#[test]
fn an_unsigned_or_wrongly_signed_hub_time_is_refused_and_the_clock_stays() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    let clock_before = store.hub_time_ms();
    let lease_before = all_remaining(&store, clock(1_000, 1));
    // Signed by the snapshot key, not the timestamp key.
    let wrong_key = w.hub_time_signed_by(&w.snap, 1, T0 + 60_000);
    assert_eq!(
        store.accept(&wrong_key, rx(1_000, 1)).unwrap_err(),
        Rejection::BadSignature
    );
    // Tampered after signing.
    let Message::HubTime(mut tampered) = w.hub_time(1, T0 + 60_000) else {
        panic!("hub time")
    };
    tampered.time.hub_ms += 1;
    assert_eq!(
        store
            .accept(&Message::HubTime(tampered), rx(1_000, 1))
            .unwrap_err(),
        Rejection::BadSignature
    );
    // Not signed at all.
    let Message::HubTime(mut empty) = w.hub_time(1, T0 + 60_000) else {
        panic!("hub time")
    };
    empty.signature.clear();
    assert_eq!(
        store
            .accept(&Message::HubTime(empty), rx(1_000, 1))
            .unwrap_err(),
        Rejection::BadSignature
    );
    // A time signed by an untrusted root's delegation.
    let stranger = World::new_with_root(9);
    assert_eq!(
        store
            .accept(&stranger.hub_time(1, T0 + 60_000), rx(1_000, 1))
            .unwrap_err(),
        Rejection::BadDelegation
    );
    assert_eq!(store.hub_time_ms(), clock_before);
    assert_eq!(all_remaining(&store, clock(1_000, 1)), lease_before);
}

#[test]
fn a_replayed_or_older_signed_time_does_not_move_the_clock_back_or_extend() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store
        .accept(&w.hub_time(1, T0 + 10 * 60_000), rx(1_000, 1))
        .unwrap();
    let held = store.hub_time_ms();
    let lease = all_remaining(&store, clock(2_000, 1));
    for old in [T0 + 10 * 60_000, T0 + 5 * 60_000, T0] {
        assert_eq!(
            store.accept(&w.hub_time(1, old), rx(2_000, 1)).unwrap_err(),
            Rejection::StaleTimestamp
        );
    }
    assert_eq!(store.hub_time_ms(), held);
    assert_eq!(all_remaining(&store, clock(2_000, 1)), lease);
}

#[test]
fn a_hub_time_for_the_wrong_node_tenant_or_an_old_epoch_is_refused() {
    let w = World::new();
    let mut store = w.store_with(snapshot(3));
    let Message::HubTime(base) = w.hub_time(3, T0 + 60_000) else {
        panic!("hub time")
    };
    let resign = |time: HubTime| Message::HubTime(sign_hub_time(time, &w.ts, w.delegation.clone()));
    let mut other_node = base.time.clone();
    other_node.node = NodeId::new("branch-2").unwrap();
    assert_eq!(
        store.accept(&resign(other_node), rx(1, 1)).unwrap_err(),
        Rejection::WrongNode
    );
    let mut other_tenant = base.time.clone();
    other_tenant.tenant = TenantId::new("other").unwrap();
    assert_eq!(
        store.accept(&resign(other_tenant), rx(1, 1)).unwrap_err(),
        Rejection::WrongTenant
    );
    assert_eq!(
        store
            .accept(&w.hub_time(2, T0 + 60_000), rx(1, 1))
            .unwrap_err(),
        Rejection::TimestampMismatch
    );
    assert_eq!(store.hub_time_ms(), T0);
}

#[test]
fn a_hub_time_with_no_snapshot_held_is_refused() {
    let w = World::new();
    let mut store = SnapshotStore::open(w.anchor(), MemoryPersistence::default(), clock(0, 1));
    assert_eq!(
        store.accept(&w.hub_time(0, T0), rx(0, 1)).unwrap_err(),
        Rejection::TimestampMismatch
    );
    assert_eq!(store.hub_time_ms(), 0);
}

#[test]
fn a_snapshot_signed_before_the_hub_clock_cannot_move_the_clock_back() {
    let w = World::new();
    let mut store = w.store_with(snapshot(1));
    store
        .accept(&w.hub_time(1, T0 + 10 * 60_000), rx(1_000, 1))
        .unwrap();
    // A new epoch whose issue time is older than the clock: accepted on its epoch, but the
    // clock stays and the lease counts from the clock, not from the older issue time.
    let got = store.accept(&w.signed(snapshot(2)), rx(2_000, 1)).unwrap();
    assert_eq!(store.hub_time_ms(), T0 + 10 * 60_000);
    assert_eq!(got.remaining_ms[0], HOUR - 10 * 60_000);
}

#[test]
fn a_persistence_failure_leaves_the_clock_where_it_was() {
    let w = World::new();
    let store = w.store_with(snapshot(1));
    let mut p = store.into_persistence();
    p.fail_saves = true;
    let mut r = reopen(&w, p, clock(0, 2));
    assert_eq!(
        r.accept(&w.hub_time(1, T0 + 60_000), rx(0, 2)).unwrap_err(),
        Rejection::PersistFailed
    );
    assert_eq!(r.hub_time_ms(), T0);
}
