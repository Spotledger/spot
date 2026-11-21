#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Restoring an intake after a restart (WP-RTB-02.T12): a restarted node must never acknowledge
//! what it has not applied, and the hub's redelivery must leave it where it would have been had it
//! never stopped.
mod common;

use common::*;
use serde_json::json;
use spot_sync_closure::{BundleRecord, RecordKey, Role};
use spot_sync_node::{
    BundleApply, MemoryParking, NodeIntake, ParkingLimits, ParkingStore, Receipt, RepairCause,
    RepairItem, RestoreError, RestoreState, ServeStamp,
};

fn stamp(seq: u64, after: u64) -> ServeStamp {
    ServeStamp { seq, after }
}

fn serve(n: &mut Intake, id: &str, position: u64, refs: &[&str], s: ServeStamp) -> Receipt {
    n.receive_served(deliver(bundle(vec![rec(id, position, refs)])), s, 0)
        .expect("ok")
}

fn skewed(id: &str, position: u64) -> BundleRecord {
    BundleRecord {
        role: Role::Root,
        key: RecordKey::new("Doc", id),
        position,
        image: json!({ "refs": [], "major": 9 }),
        deleted: false,
    }
}

/// A restarted intake: the durable stores (the applied records and the parked bundles) come back
/// as they were, everything in memory starts empty, then the restore seeds the tenant.
fn restarted(before: &Intake, state: RestoreState) -> Intake {
    let mut apply = MemApply::default();
    for ((tenant, key), version) in &before.applier().stored {
        apply.stored.insert((tenant.clone(), key.clone()), *version);
    }
    let mut parking = MemoryParking::new();
    for parked in before.parking().parked(TENANT) {
        parking.park(parked.clone());
    }
    let mut n = NodeIntake::new(
        config(ParkingLimits::default()),
        apply,
        parking,
        JsonCatalog,
        report(),
    )
    .expect("valid config");
    n.restore(TENANT, state).expect("restore");
    n
}

/// The state the durable stores hold for the scenario of `before_restart`: bundle 1 (position 10)
/// applied, bundle 2 (position 20, needs `m`) parked, bundle 3 (position 30) applied, serve
/// prefix 1 stored.
fn before_restart() -> Intake {
    let mut n = intake();
    assert!(matches!(
        serve(&mut n, "a", 10, &[], stamp(1, 0)),
        Receipt::Applied(_)
    ));
    assert!(matches!(
        serve(&mut n, "b", 20, &["m"], stamp(2, 1)),
        Receipt::Parked { .. }
    ));
    assert!(matches!(
        serve(&mut n, "c", 30, &[], stamp(3, 2)),
        Receipt::Applied(_)
    ));
    assert_eq!(n.applied_prefix(TENANT), Some(10));
    assert_eq!(n.served_prefix(TENANT), Some(1));
    n
}

fn durable_state() -> RestoreState {
    RestoreState {
        applied_prefix: Some(10),
        served_prefix: Some(1),
        holds: None,
        parked_first_positions: vec![20],
        repair: Vec::new(),
    }
}

#[test]
fn a_restored_intake_holds_the_prefix_at_a_parked_bundle() {
    let before = before_restart();
    let mut n = restarted(&before, durable_state());
    assert_eq!(n.applied_prefix(TENANT), Some(10));
    assert_eq!(n.served_prefix(TENANT), Some(1));
    // The hub redelivers bundle 3 (above the stored serve prefix): it is a replay, and the
    // prefix does not run past the parked bundle below it.
    assert!(matches!(
        serve(&mut n, "c", 30, &[], stamp(3, 2)),
        Receipt::Applied(_)
    ));
    assert_eq!(n.applied_prefix(TENANT), Some(10));
    assert_eq!(n.served_prefix(TENANT), Some(1));
}

#[test]
fn restoring_then_redelivery_equals_never_having_restarted() {
    // Never restarted: the missing record arrives and everything settles.
    let mut straight = before_restart();
    serve(&mut straight, "m", 40, &[], stamp(4, 3));

    // Restarted after bundle 3: the hub redelivers bundles 2 and 3 (above the stored prefix),
    // then the record.
    let before = before_restart();
    let mut n = restarted(&before, durable_state());
    serve(&mut n, "b", 20, &["m"], stamp(2, 1));
    serve(&mut n, "c", 30, &[], stamp(3, 2));
    assert_eq!(n.served_prefix(TENANT), Some(1));
    serve(&mut n, "m", 40, &[], stamp(4, 3));

    assert_eq!(n.applied_prefix(TENANT), straight.applied_prefix(TENANT));
    assert_eq!(n.served_prefix(TENANT), straight.served_prefix(TENANT));
    assert_eq!(n.served_prefix(TENANT), Some(4));
    for id in ["a", "b", "c", "m"] {
        assert!(n.applier().has(TENANT, &key(id), 0), "{id} applied");
    }
    assert!(n.parking().is_empty(TENANT));
}

#[test]
fn a_restored_serve_prefix_counts_only_what_comes_above_it() {
    let mut n = intake();
    n.restore(
        TENANT,
        RestoreState {
            served_prefix: Some(3),
            applied_prefix: Some(30),
            ..RestoreState::default()
        },
    )
    .expect("restore");
    // A re-served bundle at or below the floor changes nothing; the next one extends it.
    serve(&mut n, "old", 25, &[], stamp(2, 1));
    assert_eq!(n.served_prefix(TENANT), Some(3));
    serve(&mut n, "next", 31, &[], stamp(4, 3));
    assert_eq!(n.served_prefix(TENANT), Some(4));
    // A bundle that names something the node never had below it still blocks the way.
    serve(&mut n, "far", 50, &[], stamp(7, 6));
    assert_eq!(n.served_prefix(TENANT), Some(4));
}

fn repair_item(first: u64) -> RepairItem {
    RepairItem {
        tenant: TENANT.to_owned(),
        first_position: first,
        missing: vec![key("m")],
        cause: RepairCause::Expired,
    }
}

#[test]
fn a_repair_item_holds_the_prefix_until_its_bundle_applies() {
    let mut n = intake();
    n.restore(
        TENANT,
        RestoreState {
            applied_prefix: Some(10),
            served_prefix: Some(1),
            repair: vec![repair_item(20)],
            ..RestoreState::default()
        },
    )
    .expect("restore");
    assert_eq!(n.applied_prefix(TENANT), Some(10));
    // The restored item is already in the durable list: it is not queued a second time.
    assert_eq!(n.repair_len(), 0);

    // A later bundle does not carry the prefix past the item.
    serve(&mut n, "c", 30, &[], stamp(3, 2));
    assert_eq!(n.applied_prefix(TENANT), Some(10));
    assert_eq!(n.served_prefix(TENANT), Some(1));

    // The hub resends the bundle that went to repair and it applies: the hold goes.
    serve(&mut n, "b", 20, &[], stamp(2, 1));
    assert_eq!(n.applied_prefix(TENANT), Some(30));
    assert_eq!(n.served_prefix(TENANT), Some(3));
}

#[test]
fn a_restored_repair_hold_ends_when_the_resent_bundle_is_held_for_skew() {
    let mut n = intake();
    n.restore(
        TENANT,
        RestoreState {
            applied_prefix: Some(10),
            repair: vec![repair_item(20)],
            ..RestoreState::default()
        },
    )
    .expect("restore");
    let receipt = n
        .receive_served(deliver(bundle(vec![skewed("s", 20)])), stamp(2, 1), 0)
        .expect("ok");
    assert!(matches!(receipt, Receipt::Held(_)));
    // A bundle held for skew never blocks the prefix, and neither does the hold it replaces.
    assert_eq!(n.applied_prefix(TENANT), Some(20));
}

#[test]
fn two_repair_items_at_one_position_hold_it_once() {
    let mut n = intake();
    n.restore(
        TENANT,
        RestoreState {
            applied_prefix: Some(10),
            repair: vec![repair_item(20), repair_item(20)],
            ..RestoreState::default()
        },
    )
    .expect("restore");
    serve(&mut n, "b", 20, &[], stamp(1, 0));
    assert_eq!(n.applied_prefix(TENANT), Some(20));
}

#[test]
fn saved_holds_come_back() {
    let mut first = intake();
    let receipt = first
        .receive_served(deliver(bundle(vec![skewed("s", 20)])), stamp(1, 0), 0)
        .expect("ok");
    assert!(matches!(receipt, Receipt::Held(_)));
    let saved = first.holds(TENANT).expect("tenant known").clone();
    assert!(!saved.holds().is_empty());

    let mut n = intake();
    n.restore(
        TENANT,
        RestoreState {
            holds: Some(saved.clone()),
            ..RestoreState::default()
        },
    )
    .expect("restore");
    assert_eq!(n.holds(TENANT), Some(&saved));
}

#[test]
fn a_restore_comes_first_and_only_once() {
    let mut n = intake();
    serve(&mut n, "a", 10, &[], stamp(1, 0));
    assert_eq!(
        n.restore(TENANT, RestoreState::default()),
        Err(RestoreError::AlreadyStarted)
    );

    let mut fresh = intake();
    fresh
        .restore(TENANT, RestoreState::default())
        .expect("restore");
    assert_eq!(
        fresh.restore(TENANT, RestoreState::default()),
        Err(RestoreError::AlreadyStarted)
    );
}

#[test]
fn another_tenants_repair_item_is_refused_whole() {
    let mut n = intake();
    let mut item = repair_item(20);
    item.tenant = "other".to_owned();
    assert_eq!(
        n.restore(
            TENANT,
            RestoreState {
                repair: vec![repair_item(10), item],
                ..RestoreState::default()
            }
        ),
        Err(RestoreError::Invalid)
    );
    assert_eq!(n.tenant_count(), 0);
    assert_eq!(n.applied_prefix(TENANT), None);
}

#[test]
fn the_tenant_cap_holds_for_a_restore() {
    let mut n = intake();
    for i in 0..8 {
        n.restore(&format!("t{i}"), RestoreState::default())
            .expect("within the cap");
    }
    assert_eq!(
        n.restore("t8", RestoreState::default()),
        Err(RestoreError::TooManyTenants)
    );
    assert_eq!(n.tenant_count(), 8);
}

#[test]
fn restore_code_is_the_catalogued_one() {
    assert_eq!(
        RestoreError::AlreadyStarted.code(),
        "sync_node_intake.restore_refused"
    );
}
