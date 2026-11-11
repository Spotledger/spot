//! The writer trait: the in-memory reference, the bounded scan, and error propagation.

// Test code may unwrap and expect what the test itself built.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{block_on, doc, rid, schema, set};
use spot_app_api::prelude::Value;
use spot_edge_sync::{
    Edge, EdgeChanges, EdgeSync, EdgeWriter, LifecycleHook, MemoryEdgeWriter, apply_in, edges_of,
};
use spot_storage_contract::NodeRef;

fn order_with_customer(id: u64, customer: u64) -> spot_app_api::Document {
    let mut order = doc("order", rid(id));
    set(&mut order, "customer", Value::Id(rid(customer)));
    order
}

fn node(entity: &str, id: u64) -> NodeRef {
    NodeRef {
        entity: common::name(entity),
        id: rid(id),
    }
}

#[test]
fn scan_by_source_returns_only_that_records_edges() {
    let schema = schema();
    let mut writer = MemoryEdgeWriter::new();
    let first = order_with_customer(1, 2);
    let second = order_with_customer(3, 4);
    for edge in edges_of(&schema, &first)
        .into_iter()
        .chain(edges_of(&schema, &second))
    {
        block_on(writer.add(&edge)).expect("in-memory add");
    }
    let scanned = block_on(writer.scan_by_source(&node("order", 1))).expect("scan");
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].owner, node("order", 1));
    assert_eq!(scanned[0].to, node("party", 2));
}

#[test]
fn a_writer_error_from_apply_in_surfaces() {
    let schema = schema();
    let order = order_with_customer(1, 2);
    let changes = EdgeChanges::between(
        &std::collections::BTreeSet::new(),
        &edges_of(&schema, &order),
    );
    let mut writer = common::FailingWriter::default();
    let result = block_on(apply_in(&mut writer, &changes));
    assert!(result.is_err());
    assert_eq!(
        result.expect_err("must fail").code(),
        "edge_sync.write_failed"
    );
}

#[test]
fn the_hook_surfaces_a_writer_error() {
    let schema = schema();
    let order = order_with_customer(1, 2);
    let mut writer = common::FailingWriter::default();
    let result = block_on(EdgeSync::new().after_write(&schema, None, Some(&order), &mut writer));
    assert_eq!(
        result.expect_err("must fail").code(),
        "edge_sync.write_failed"
    );
}

#[test]
fn from_edges_and_get_round_trip() {
    let schema = schema();
    let order = order_with_customer(1, 2);
    let edges: Vec<Edge> = edges_of(&schema, &order).into_iter().collect();
    let writer = MemoryEdgeWriter::from_edges(edges.clone());
    assert_eq!(writer.len(), 1);
    assert_eq!(writer.all(), edges.iter().cloned().collect());
    assert!(writer.get(&edges[0].key()).is_some());
}
