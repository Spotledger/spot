//! The invariant checker and the repair: after a synced change the drift is empty, and injected
//! drift is reported and repaired.

// Test code may unwrap and expect what the test itself built.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{block_on, doc, rid, schema, set};
use spot_app_api::prelude::Value;
use spot_edge_sync::{
    Drift, EdgeChanges, EdgeSync, LifecycleHook, MemoryEdgeWriter, check_invariant, edges_of,
    repair,
};
use spot_storage_contract::NodeRef;

/// Applies a document change through the hook, exactly as the engine's post-write seam would.
fn sync(
    hook: &EdgeSync,
    schema: &spot_edge_sync::EdgeSchema,
    writer: &mut MemoryEdgeWriter,
    old: Option<&spot_app_api::Document>,
    new: Option<&spot_app_api::Document>,
) {
    block_on(hook.after_write(schema, old, new, writer)).expect("the in-memory writer never fails");
}

fn customer_order(id: u64, customer: u64) -> spot_app_api::Document {
    let mut order = doc("order", rid(id));
    set(&mut order, "customer", Value::Id(rid(customer)));
    order
}

#[test]
fn a_synced_change_leaves_no_drift() {
    let schema = schema();
    let hook = EdgeSync::new();
    let mut writer = MemoryEdgeWriter::new();
    let order = customer_order(1, 2);
    sync(&hook, &schema, &mut writer, None, Some(&order));
    assert!(
        check_invariant(&schema, [&order], writer.all()).is_empty(),
        "create must leave the invariant clean"
    );

    let mut moved = order.clone();
    set(&mut moved, "customer", Value::Id(rid(3)));
    sync(&hook, &schema, &mut writer, Some(&order), Some(&moved));
    assert!(check_invariant(&schema, [&moved], writer.all()).is_empty());

    sync(&hook, &schema, &mut writer, Some(&moved), None);
    assert!(writer.is_empty(), "delete removes every edge");
    assert!(check_invariant(&schema, [], writer.all()).is_empty());
}

#[test]
fn missing_extra_and_mismatch_drift_are_each_reported() {
    let schema = schema();
    let order = customer_order(1, 2);
    let desired = edges_of(&schema, &order);
    let only = desired.iter().next().cloned().expect("one edge");

    // Missing: the documents imply the edge, storage has none.
    let drift = check_invariant(&schema, [&order], Vec::new());
    assert_eq!(drift.len(), 1);
    assert!(matches!(drift[0], Drift::Missing { .. }));
    assert_eq!(drift[0].code(), "edge_sync.missing_edge");

    // Extra: storage has an edge no document implies.
    let mut wrong_source = only.clone();
    wrong_source.from = NodeRef {
        entity: common::name("party"),
        id: rid(99),
    };
    let drift = check_invariant(&schema, [&order], [wrong_source.clone()]);
    assert_eq!(
        drift.len(),
        2,
        "the right edge is missing, the wrong one extra"
    );
    assert!(drift.iter().any(|d| matches!(d, Drift::Extra { .. })));

    // Mismatch: same key, different properties.
    let mut changed = only.clone();
    changed
        .properties
        .insert("role".into(), spot_edge_sync::PropValue::Text("x".into()));
    let drift = check_invariant(&schema, [&order], [changed]);
    assert_eq!(drift.len(), 1);
    assert!(matches!(drift[0], Drift::Mismatch { .. }));
    assert_eq!(drift[0].code(), "edge_sync.edge_mismatch");
}

#[test]
fn repair_returns_the_change_that_fixes_drift() {
    let schema = schema();
    let order = customer_order(1, 2);
    let expected = edges_of(&schema, &order);
    let right = expected.iter().next().cloned().expect("one edge");
    let mut extra = right.clone();
    extra.to = NodeRef {
        entity: common::name("party"),
        id: rid(42),
    };

    // Storage is missing the right edge and holds an extra one.
    let changes = repair(&schema, [&order], [extra.clone()]);
    assert_eq!(changes.added, expected);
    assert_eq!(changes.removed, [extra].into_iter().collect());

    // Applying the repair through the writer clears the drift.
    let mut writer = MemoryEdgeWriter::new();
    block_on(spot_edge_sync::apply_in(&mut writer, &changes)).expect("in-memory apply");
    assert!(check_invariant(&schema, [&order], writer.all()).is_empty());
}

#[test]
fn repair_updates_a_mismatched_edge() {
    let schema = schema();
    let mut task = doc("task_link", rid(9));
    set(&mut task, "order", Value::Id(rid(1)));
    set(&mut task, "project", Value::Id(rid(7)));
    set(&mut task, "role", Value::Text("owner".into()));
    let expected = edges_of(&schema, &task);
    let mut stale = expected.iter().next().cloned().expect("one edge");
    stale
        .properties
        .insert("role".into(), spot_edge_sync::PropValue::Text("old".into()));

    let changes = repair(&schema, [&task], [stale.clone()]);
    assert_eq!(changes.updated.len(), 1);
    assert_eq!(changes.updated[0].before, stale);
    assert!(changes.added.is_empty());
    assert!(changes.removed.is_empty());
    assert_eq!(
        changes.updated[0].after,
        *expected.iter().next().expect("one edge")
    );
}

#[test]
fn apply_to_matches_the_writer_behaviour() {
    let schema = schema();
    let order = customer_order(1, 2);
    let mut set_edges = edges_of(&schema, &order);
    let mut writer = MemoryEdgeWriter::new();
    let changes = EdgeChanges::between(&std::collections::BTreeSet::new(), &set_edges);
    block_on(spot_edge_sync::apply_in(&mut writer, &changes)).expect("apply");
    changes.apply_to(&mut set_edges);
    assert_eq!(set_edges, writer.all());
}
