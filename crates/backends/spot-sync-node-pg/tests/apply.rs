//! The real apply on a real PostgreSQL: one transaction per bundle, ownership and shape routing,
//! idempotence, tombstones, no event, crash points, tenant isolation, the 500-bundle replay in a
//! random order and the 10,000-record catch-up. Gated on `SPOT_TEST_PG_URL` (skipped with a
//! message when unset, a failure when `SPOT_REQUIRE_PG=1`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use std::time::Instant;

use common::{World, bundle, put, put_bad_shape, tomb, world};
use spot_storage_contract::RecordId;
use spot_sync_closure::RecordKey;
use spot_sync_node::BundleApply;
use spot_sync_node_pg::Failpoint;
use spot_sync_protocol::{Checkpoint, CheckpointStore};

fn key(id: RecordId) -> RecordKey {
    RecordKey::new("item", id.to_string())
}

fn ledger_rows(w: &World) -> i64 {
    w.scalar("SELECT count(*) FROM spot_sync_node_applied")
}

#[test]
fn a_bundle_is_stored_with_its_ledger_rows_and_emits_no_event() {
    let Some(w) = world() else { return };
    let mut apply = w.apply(common::TENANT);
    let ids: Vec<RecordId> = (0..100).map(|_| RecordId::new_v7()).collect();
    let records = ids
        .iter()
        .enumerate()
        .map(|(i, id)| put(*id, &format!("t{i}"), u64::try_from(i).unwrap() + 1))
        .collect();
    let set = apply
        .apply_bundle(common::TENANT, &bundle(records))
        .unwrap();
    assert_eq!(set.records.len(), 100);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(w.stored(*id).unwrap().1, format!("t{i}"));
        assert!(apply.has(common::TENANT, &key(*id), 0));
    }
    assert_eq!(ledger_rows(&w), 100);
    // A replicated record is not a new change here: the node's outbox stays empty.
    assert_eq!(w.scalar("SELECT count(*) FROM spot_outbox"), 0);
    // Every record left its `replicated` audit marker.
    assert_eq!(w.audit.len(), 100);
}

#[test]
fn a_replay_is_a_no_op_and_an_older_version_never_rolls_data_back() {
    let Some(w) = world() else { return };
    let mut apply = w.apply(common::TENANT);
    let id = RecordId::new_v7();
    apply
        .apply_bundle(common::TENANT, &bundle(vec![put(id, "v5", 5)]))
        .unwrap();
    let rows = ledger_rows(&w);
    // The same bundle again: settled, nothing written.
    apply
        .apply_bundle(common::TENANT, &bundle(vec![put(id, "v5", 5)]))
        .unwrap();
    // An older version: Stale, the stored data is the newer one.
    apply
        .apply_bundle(common::TENANT, &bundle(vec![put(id, "v3", 3)]))
        .unwrap();
    assert_eq!(w.stored(id).unwrap().1, "v5");
    assert_eq!(ledger_rows(&w), rows);
    assert!(apply.has(common::TENANT, &key(id), 5));
    assert!(!apply.has(common::TENANT, &key(id), 6));
}

#[test]
fn one_non_owned_record_refuses_the_whole_bundle_and_quarantines_it() {
    let Some(w) = world() else { return };
    let mut apply = w.apply(common::TENANT);
    let (good, bad) = (RecordId::new_v7(), RecordId::new_v7());
    let refusal = apply
        .apply_bundle(
            common::TENANT,
            &bundle(vec![put(good, "fine", 1), put(bad, "not-mine", 2)]),
        )
        .unwrap_err();
    assert!(refusal.reason.contains("replica_apply.not_owner"));
    // No partial bundle is ever visible: not the good record, not its ledger row, not the bad one.
    assert!(w.stored(good).is_none());
    assert!(w.stored(bad).is_none());
    assert_eq!(ledger_rows(&w), 0);
    assert_eq!(w.quarantine.len(), 1);
    // The refusal names a code, never a value.
    assert!(!refusal.reason.contains("not-mine"));
}

#[test]
fn a_shape_error_is_quarantined_with_its_code_and_rolls_the_bundle_back() {
    let Some(w) = world() else { return };
    let mut apply = w.apply(common::TENANT);
    let (good, bad) = (RecordId::new_v7(), RecordId::new_v7());
    let refusal = apply
        .apply_bundle(
            common::TENANT,
            &bundle(vec![put(good, "fine", 1), put_bad_shape(bad, 2)]),
        )
        .unwrap_err();
    assert!(refusal.reason.contains("replica_apply.unknown_field"));
    assert!(w.stored(good).is_none());
    assert_eq!(ledger_rows(&w), 0);
    assert_eq!(w.quarantine.len(), 1);
}

#[test]
fn a_record_whose_image_is_another_record_is_refused() {
    let Some(w) = world() else { return };
    let mut apply = w.apply(common::TENANT);
    let (claimed, actual) = (RecordId::new_v7(), RecordId::new_v7());
    let mut record = put(actual, "x", 1);
    record.key = key(claimed);
    let refusal = apply
        .apply_bundle(common::TENANT, &bundle(vec![record]))
        .unwrap_err();
    assert!(refusal.reason.contains("invalid_identifier"));
    assert!(w.stored(actual).is_none());
    assert_eq!(ledger_rows(&w), 0);
}

#[test]
fn a_tombstone_deletes_in_the_bundle_and_blocks_an_older_image() {
    let Some(w) = world() else { return };
    let mut apply = w.apply(common::TENANT);
    let id = RecordId::new_v7();
    apply
        .apply_bundle(common::TENANT, &bundle(vec![put(id, "v5", 5)]))
        .unwrap();
    apply
        .apply_bundle(common::TENANT, &bundle(vec![tomb(id, 6)]))
        .unwrap();
    assert!(w.stored(id).is_none());
    assert!(!apply.has(common::TENANT, &key(id), 0));
    assert!(apply.has_tombstone(common::TENANT, &key(id), 6));
    // A replay of the old image does not bring the record back.
    apply
        .apply_bundle(common::TENANT, &bundle(vec![put(id, "v5", 5)]))
        .unwrap();
    assert!(w.stored(id).is_none());
    // A later re-creation does.
    apply
        .apply_bundle(common::TENANT, &bundle(vec![put(id, "v9", 9)]))
        .unwrap();
    assert_eq!(w.stored(id).unwrap().1, "v9");
    assert!(apply.has(common::TENANT, &key(id), 0));
    // An older delete does not remove the record re-created later.
    apply
        .apply_bundle(common::TENANT, &bundle(vec![tomb(id, 7)]))
        .unwrap();
    assert_eq!(w.stored(id).unwrap().1, "v9");
    // Deleting a record never held is not an error and is remembered.
    let never = RecordId::new_v7();
    apply
        .apply_bundle(common::TENANT, &bundle(vec![tomb(never, 4)]))
        .unwrap();
    assert!(apply.has_tombstone(common::TENANT, &key(never), 4));
}

#[test]
fn a_crash_before_the_commit_leaves_neither_the_data_the_ledger_nor_the_checkpoint() {
    let Some(w) = world() else { return };
    let apply = w.apply(common::TENANT);
    let source = common::hub_source();
    let id = RecordId::new_v7();
    let b = bundle(vec![put(id, "a", 1), put(RecordId::new_v7(), "b", 2)]);
    let checkpoint = Checkpoint::new(source.clone(), 2);
    for point in [Failpoint::AfterRecords, Failpoint::BeforeCommit] {
        w.crash.arm(Some(point));
        let refusal = apply
            .apply_bundle_checkpointed(common::TENANT, &b, &checkpoint)
            .unwrap_err();
        assert!(refusal.reason.contains("failpoint"));
        assert!(w.stored(id).is_none(), "{point:?}");
        assert_eq!(ledger_rows(&w), 0, "{point:?}");
        assert_eq!(
            w.scalar("SELECT count(*) FROM spot_sync_node_checkpoint"),
            0
        );
    }
    // Without the crash both exist, together.
    w.crash.arm(None);
    apply
        .apply_bundle_checkpointed(common::TENANT, &b, &checkpoint)
        .unwrap();
    assert!(w.stored(id).is_some());
    assert_eq!(ledger_rows(&w), 2);
    let stored = spot_sync_node_pg::PgCheckpoints::new(w.db.clone(), common::TENANT).unwrap();
    assert_eq!(stored.load(&source).unwrap().last_applied_seq, 2);
}

#[test]
fn a_checkpoint_ahead_of_the_bundle_refuses_it_whole() {
    let Some(w) = world() else { return };
    let apply = w.apply(common::TENANT);
    let id = RecordId::new_v7();
    let refusal = apply
        .apply_bundle_checkpointed(
            common::TENANT,
            &bundle(vec![put(id, "a", 1)]),
            &Checkpoint::new(common::hub_source(), 9),
        )
        .unwrap_err();
    assert!(refusal.reason.contains("checkpoint_ahead"));
    assert!(w.stored(id).is_none());
    assert_eq!(ledger_rows(&w), 0);
}

#[test]
fn tenants_never_cross() {
    let Some(w) = world() else { return };
    let mut a = w.apply("tenant_a");
    let mut b = w.apply("tenant_b");
    let id = RecordId::new_v7();
    a.apply_bundle("tenant_a", &bundle(vec![put(id, "a", 1)]))
        .unwrap();
    assert!(a.has("tenant_a", &key(id), 0));
    assert!(!b.has("tenant_b", &key(id), 0));
    // Each apply answers for its own tenant only.
    assert!(!a.has("tenant_b", &key(id), 0));
    let refusal = a.apply_bundle("tenant_b", &bundle(vec![put(RecordId::new_v7(), "x", 1)]));
    assert!(refusal.is_err());
    let other = RecordId::new_v7();
    b.apply_bundle("tenant_b", &bundle(vec![put(other, "b", 1)]))
        .unwrap();
    assert_eq!(
        w.scalar("SELECT count(*) FROM spot_sync_node_applied WHERE tenant = 'tenant_a'"),
        1
    );
    assert_eq!(
        w.scalar("SELECT count(*) FROM spot_sync_node_applied WHERE tenant = 'tenant_b'"),
        1
    );
}

/// A small deterministic shuffle (no random source in the tests either).
fn shuffled(mut items: Vec<usize>, mut state: u64) -> Vec<usize> {
    for i in (1..items.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = usize::try_from(state >> 33).unwrap() % (i + 1);
        items.swap(i, j);
    }
    items
}

#[test]
fn five_hundred_bundles_replayed_in_a_random_order_end_in_the_in_order_state() {
    let Some(w) = world() else { return };
    let mut in_order = w.apply("in_order");
    let mut shuffled_apply = w.apply("shuffled");
    let ids_a: Vec<RecordId> = (0..100).map(|_| RecordId::new_v7()).collect();
    let ids_b: Vec<RecordId> = (0..100).map(|_| RecordId::new_v7()).collect();
    // Bundle i rewrites record i % 100 at position i + 1: the last bundle of each record wins.
    let history = |ids: &[RecordId], i: usize| {
        bundle(vec![put(
            ids[i % 100],
            &format!("v{}", i + 1),
            u64::try_from(i).unwrap() + 1,
        )])
    };
    for i in 0..500 {
        in_order
            .apply_bundle("in_order", &history(&ids_a, i))
            .unwrap();
    }
    for i in shuffled((0..500).collect(), 0x5eed) {
        shuffled_apply
            .apply_bundle("shuffled", &history(&ids_b, i))
            .unwrap();
    }
    for k in 0..100 {
        let a = w.stored(ids_a[k]).unwrap();
        let b = w.stored(ids_b[k]).unwrap();
        assert_eq!(a.1, b.1, "record {k}: the same final image");
        assert_eq!(a.1, format!("v{}", 400 + k + 1));
    }
}

#[test]
fn ten_thousand_records_catch_up_in_order_within_budget() {
    let Some(w) = world() else { return };
    let mut apply = w.apply(common::TENANT);
    let start = Instant::now();
    let mut position = 0_u64;
    let mut last = RecordId::new_v7();
    for _ in 0..20 {
        let records = (0..500)
            .map(|_| {
                position += 1;
                last = RecordId::new_v7();
                put(last, "catch-up", position)
            })
            .collect();
        apply
            .apply_bundle(common::TENANT, &bundle(records))
            .unwrap();
    }
    let elapsed = start.elapsed();
    println!("CATCHUP 10000 records in 20 bundles: {elapsed:?}");
    assert_eq!(ledger_rows(&w), 10_000);
    assert!(w.stored(last).is_some());
    // The budget: a replica of ten thousand records catches up inside two minutes.
    assert!(elapsed.as_secs() < 120, "catch-up took {elapsed:?}");
}
