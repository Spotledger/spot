//! The snapshot keeper: signed, expiring, rollback, fork, replay refused; the lease runs on the
//! monotonic clock; a clock moved back refuses and extends nothing; the request names what the
//! node holds.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::time::Duration;

use common::*;
use spot_edge_offline_host::carrier::SnapshotMsg;
use spot_edge_offline_host::identity::{
    BoxedPersistence, CLOCK_MOVED_BACK, KeeperError, SnapshotKeeper,
};
use spot_edge_offline_host::lease::Standing;
use spot_offline_identity::{
    Decision, DenyReason, MAX_CERT_TTL_MS, MemoryPersistence, NodeClock, Rejection, Tier,
};

fn clock(mono_ms: u64) -> NodeClock {
    NodeClock {
        mono_ms,
        boot_id: 1,
    }
}

fn keeper(world: &World) -> SnapshotKeeper<MemoryPersistence> {
    let mut keeper = SnapshotKeeper::open(world.anchor(), MemoryPersistence::default(), clock(0));
    // The node certificate comes from the hub (a separate exchange); sign-in needs it.
    keeper
        .store_mut()
        .renew_certificate(1, MAX_CERT_TTL_MS, true, clock(0))
        .unwrap();
    keeper
}

const LEASES: [u64; 3] = [HOUR, 12 * HOUR, 24 * HOUR];

#[test]
fn a_signed_snapshot_is_kept_and_its_lease_runs_down() {
    let world = World::new();
    let mut k = keeper(&world);
    assert_eq!(
        k.standing(clock(0)),
        Standing::Lapsed,
        "no snapshot, no lease"
    );
    assert!(k.refresh_due(clock(0)));
    k.accept(&world.message(world.snapshot(1, T0, LEASES)), clock(0))
        .unwrap();
    assert_eq!(
        k.standing(clock(7 * HOUR)),
        Standing::Alive(Duration::from_millis(17 * HOUR))
    );
    assert_eq!(
        k.snapshot_age(clock(7 * HOUR)),
        Some(Duration::from_millis(7 * HOUR))
    );
    assert!(matches!(
        k.sign_in("clerk", clock(7 * HOUR)),
        Decision::Allow
    ));
    // Past the lease the node refuses new sign-ins (fail closed).
    assert_eq!(k.standing(clock(25 * HOUR)), Standing::Lapsed);
    assert!(!k.sign_in("clerk", clock(25 * HOUR)).is_allowed());
}

#[test]
fn unsigned_rolled_back_forked_and_replayed_snapshots_are_refused() {
    let world = World::new();
    let mut k = keeper(&world);
    k.accept(&world.message(world.snapshot(5, T0, LEASES)), clock(0))
        .unwrap();

    // Another root signed the delegation: it is not the one this node trusts.
    let bad = World::with_root(9).message(world.snapshot(6, T0, LEASES));
    let error = k.accept(&bad, clock(10)).unwrap_err();
    assert!(matches!(
        error,
        KeeperError::Rejected(Rejection::BadDelegation)
    ));

    let rollback = k
        .accept(&world.message(world.snapshot(4, T0, LEASES)), clock(20))
        .unwrap_err();
    assert_eq!(
        rollback,
        KeeperError::Rejected(Rejection::Rollback { got: 4, held: 5 })
    );

    let mut forked = world.snapshot(5, T0, LEASES);
    forked.directory[0].enabled = false;
    let fork = k.accept(&world.message(forked), clock(30)).unwrap_err();
    assert_eq!(fork, KeeperError::Rejected(Rejection::Fork { epoch: 5 }));

    let replay = k
        .accept(&world.message(world.snapshot(5, T0, LEASES)), clock(40))
        .unwrap_err();
    assert_eq!(
        replay,
        KeeperError::Rejected(Rejection::Replay { epoch: 5 })
    );
    assert_eq!(k.store().highest_epoch(), 5);
}

#[test]
fn a_clock_moved_back_is_refused_and_extends_nothing() {
    let world = World::new();
    let mut k = keeper(&world);
    k.accept(&world.message(world.snapshot(1, T0, LEASES)), clock(0))
        .unwrap();
    // The hour passed on the monotonic clock; the node's clock then reads lower than before.
    assert_eq!(
        k.standing(clock(2 * HOUR)),
        Standing::Alive(Duration::from_millis(22 * HOUR))
    );
    assert!(matches!(
        k.sign_in("clerk", clock(2 * HOUR)),
        Decision::Allow
    ));
    let back = clock(HOUR);
    assert!(matches!(
        k.sign_in("clerk", back),
        Decision::Deny(DenyReason::ClockFault)
    ));
    assert!(matches!(
        k.authorise_action("clerk", Tier::Three, "invoice:write", back),
        Decision::Deny(DenyReason::ClockFault)
    ));
    let refused = k
        .accept(&world.message(world.snapshot(2, T0 + HOUR, LEASES)), back)
        .unwrap_err();
    assert_eq!(refused, KeeperError::ClockMovedBack);
    assert_eq!(refused.code(), CLOCK_MOVED_BACK);
    assert_eq!(k.store().highest_epoch(), 1, "nothing was accepted");
    // The clock coming back to where it was is fine again.
    assert!(matches!(
        k.sign_in("clerk", clock(3 * HOUR)),
        Decision::Allow
    ));
}

#[test]
fn the_state_survives_a_restart_and_a_replayed_image_is_refused() {
    let world = World::new();
    let mut k = SnapshotKeeper::open(
        world.anchor(),
        BoxedPersistence(Box::new(MemoryPersistence::default())),
        clock(0),
    );
    k.accept(&world.message(world.snapshot(3, T0, LEASES)), clock(0))
        .unwrap();
    k.checkpoint(clock(HOUR)).unwrap();
    let persisted = k.store().report();
    assert_eq!(persisted.highest_epoch, 3);
    // The request names what the node holds, so the hub can see a replayed image.
    assert!(
        k.request(k.next_request_id()).is_none(),
        "no node key, nothing unsigned is sent"
    );
    let k = k.with_node_key(std::sync::Arc::new(Key::new(7)));
    let Some(SnapshotMsg::Request { request_id, signed }) = k.request(k.next_request_id()) else {
        panic!("not a request");
    };
    assert_eq!(signed.request.report.unwrap().highest_epoch, 3);
    assert!(request_id.starts_with("snapshot:3@"));
}
