//! The receiving side: what an applied, parked, held and foreign bundle does to the prefix the
//! node acknowledges, and that the cursor survives a restart.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_sync_closure::RecordKey;
use spot_sync_node_host::Intake;
use support::*;

#[test]
fn applied_bundles_advance_the_prefix_and_the_cursor() {
    let apply = SharedApply::default();
    let cursor = MemCursor::default();
    let mut applier = intake(apply.clone(), cursor.clone());
    assert_eq!(applier.cursor().unwrap(), 0);
    let items = vec![
        served(1, 0, &bundle("Customer", "c1", 10, &[])),
        served(2, 1, &bundle("Customer", "c2", 11, &[])),
    ];
    let received = applier.receive(items, 0, 1_000).unwrap();
    assert_eq!(received.applied, 2);
    assert_eq!(received.prefix, 2);
    assert_eq!(*cursor.0.lock().unwrap(), 2);
    assert_eq!(apply.0.lock().unwrap().stored.len(), 2);
}

#[test]
fn a_parked_bundle_holds_the_prefix_back_until_its_reference_arrives() {
    let apply = SharedApply::default();
    let cursor = MemCursor::default();
    let mut applier = intake(apply.clone(), cursor.clone());
    // Bundle 2 references Customer/c1, which only bundle 3 brings.
    let items = vec![
        served(1, 0, &bundle("Customer", "c0", 5, &[])),
        served(2, 1, &bundle("Invoice", "i1", 6, &[("Customer", "c1")])),
        served(3, 2, &bundle("Customer", "c1", 7, &[])),
    ];
    let received = applier.receive(items, 0, 1_000).unwrap();
    // Bundle 3 applied and released the parked bundle 2: the prefix reaches 3.
    assert_eq!(received.prefix, 3);
    assert!(
        apply
            .0
            .lock()
            .unwrap()
            .stored
            .contains_key(&(TENANT.to_owned(), RecordKey::new("Invoice", "i1")))
    );

    // Alone, a parked bundle holds the prefix at the bundle below it.
    let apply = SharedApply::default();
    let cursor = MemCursor::default();
    let mut applier = intake(apply, cursor.clone());
    let items = vec![
        served(1, 0, &bundle("Customer", "c0", 5, &[])),
        served(
            2,
            1,
            &bundle("Invoice", "i1", 6, &[("Customer", "missing")]),
        ),
    ];
    let received = applier.receive(items, 0, 1_000).unwrap();
    assert_eq!(received.parked, 1);
    assert_eq!(received.prefix, 1);
    assert_eq!(*cursor.0.lock().unwrap(), 1);
}

#[test]
fn a_bundle_held_for_version_skew_does_not_block_the_stream() {
    let mut applier = intake(SharedApply::default(), MemCursor::default());
    let items = vec![
        served(1, 0, &bundle_major("Customer", "new", 5, 9)),
        served(2, 1, &bundle("Customer", "c2", 6, &[])),
    ];
    let received = applier.receive(items, 0, 1_000).unwrap();
    assert_eq!(received.held, 1);
    assert_eq!(received.applied, 1);
    assert_eq!(received.prefix, 2);
}

#[test]
fn another_tenants_bundle_is_quarantined_never_applied_nor_acknowledged() {
    let apply = SharedApply::default();
    let mut applier = intake(apply.clone(), MemCursor::default());
    let record = spot_sync_node_host::envelope::encode(
        1,
        "someone-else",
        0,
        &bundle("Customer", "x", 5, &[]),
    )
    .unwrap();
    let foreign = spot_sync_node_host::envelope::decode(&record).unwrap();
    let received = applier.receive(vec![foreign], 1, 1_000).unwrap();
    assert_eq!(
        received.quarantined, 2,
        "one foreign tenant plus one undecodable"
    );
    assert_eq!(received.applied, 0);
    assert_eq!(received.prefix, 0);
    assert!(apply.0.lock().unwrap().stored.is_empty());
}

#[test]
fn a_restart_resumes_from_the_stored_cursor_and_a_replay_is_harmless() {
    let apply = SharedApply::default();
    let cursor = MemCursor::default();
    {
        let mut applier = intake(apply.clone(), cursor.clone());
        applier
            .receive(
                vec![served(1, 0, &bundle("Customer", "c1", 5, &[]))],
                0,
                1_000,
            )
            .unwrap();
    }
    // A new applier (a restart) reads the cursor, and a re-served bundle 1 changes nothing.
    let mut applier = intake(apply.clone(), cursor.clone());
    assert_eq!(applier.cursor().unwrap(), 1);
    let received = applier
        .receive(
            vec![served(1, 0, &bundle("Customer", "c1", 5, &[]))],
            0,
            2_000,
        )
        .unwrap();
    assert_eq!(received.prefix, 1);
    assert_eq!(
        apply.0.lock().unwrap().calls,
        1,
        "the replay was not applied twice"
    );
}

#[test]
fn a_refused_bundle_is_counted_and_holds_the_prefix() {
    let apply = SharedApply::default();
    apply.0.lock().unwrap().refuse = true;
    let mut applier = intake(apply, MemCursor::default());
    let received = applier
        .receive(
            vec![served(1, 0, &bundle("Customer", "c1", 5, &[]))],
            0,
            1_000,
        )
        .unwrap();
    assert_eq!(received.refused, 1);
    assert_eq!(received.prefix, 0);
}
