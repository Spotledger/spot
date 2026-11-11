//! The incremental path: add, remove and update, idempotent by sequence number, a hole marks the
//! graph stale, and the accounted bytes match a fresh recomputation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_graph_index::{Applied, EdgeEvent, EdgeKey, GraphIndex, GraphKey};
use spot_storage_contract::{Direction, NodeRef, RecordId, Value};
use support::node;

fn ids() -> (RecordId, RecordId, RecordId) {
    (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7())
}

fn add(seq: u64, key: &EdgeKey) -> EdgeEvent {
    EdgeEvent::bare(
        seq,
        support::graph_key(),
        spot_graph_index::EdgeOp::Add,
        key.clone(),
    )
}

fn remove(seq: u64, key: &EdgeKey) -> EdgeEvent {
    EdgeEvent::bare(
        seq,
        support::graph_key(),
        spot_graph_index::EdgeOp::Remove,
        key.clone(),
    )
}

fn update(seq: u64, key: &EdgeKey, properties: &[(&str, Value)]) -> EdgeEvent {
    EdgeEvent {
        sequence: seq,
        graph: support::graph_key(),
        op: spot_graph_index::EdgeOp::Update,
        edge: key.clone(),
        properties: properties
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect(),
    }
}

#[test]
fn add_remove_and_readd_are_idempotent() {
    let (po, grn, _) = ids();
    let key = EdgeKey::new("grn.po", node("grn", grn), node("po", po));
    let mut index = GraphIndex::new(support::graph_key());

    assert_eq!(index.apply(&add(1, &key)).unwrap(), Applied::Added);
    assert_eq!(index.node_count(), 2);
    assert_eq!(index.edge_count(), 1);
    assert_eq!(index.watermark(), 1);

    // A redelivered duplicate is ignored and changes nothing.
    assert_eq!(index.apply(&add(1, &key)).unwrap(), Applied::Ignored);
    assert_eq!(index.watermark(), 1);

    assert_eq!(index.apply(&remove(2, &key)).unwrap(), Applied::Removed);
    assert_eq!(index.edge_count(), 0);
    assert_eq!(index.node_count(), 0, "an isolated endpoint is dropped");
    assert_eq!(index.apply(&remove(2, &key)).unwrap(), Applied::Ignored);

    assert_eq!(index.apply(&add(3, &key)).unwrap(), Applied::Added);
    assert_eq!(index.watermark(), 3);
}

#[test]
fn update_replaces_properties_and_is_idempotent() {
    let (po, grn, _) = ids();
    let key = EdgeKey::new("grn.po", node("grn", grn), node("po", po));
    let mut index = GraphIndex::new(support::graph_key());

    index.apply(&add(1, &key)).unwrap();
    let updated = update(2, &key, &[("qty", Value::Int(5))]);
    assert_eq!(index.apply(&updated).unwrap(), Applied::Updated);
    assert_eq!(
        index.edge_properties(&key).unwrap().get("qty"),
        Some(&Value::Int(5))
    );
    // The same update at the next sequence finds the state already matching: a no-op.
    let again = update(3, &key, &[("qty", Value::Int(5))]);
    assert_eq!(index.apply(&again).unwrap(), Applied::Unchanged);
    assert_eq!(index.watermark(), 3);

    // An update to an edge that does not exist creates it, so a redelivered update is never lost.
    let other = EdgeKey::new("invoice.grn", node("invoice", po), node("grn", grn));
    assert_eq!(
        index.apply(&update(4, &other, &[])).unwrap(),
        Applied::Added
    );
}

#[test]
fn a_gap_marks_the_graph_stale() {
    let (po, grn, _) = ids();
    let key = EdgeKey::new("grn.po", node("grn", grn), node("po", po));
    let mut index = GraphIndex::new(support::graph_key());
    index.apply(&add(1, &key)).unwrap();

    let error = index.apply(&add(3, &key)).unwrap_err();
    assert_eq!(error.code(), spot_graph_index::GAP);
    assert!(index.is_stale());
    assert_eq!(index.watermark(), 1, "the hole did not move the watermark");

    // A stale graph applies nothing until it is rebuilt.
    assert_eq!(index.apply(&add(2, &key)).unwrap(), Applied::Ignored);
    assert_eq!(index.apply(&add(4, &key)).unwrap(), Applied::Ignored);
}

#[test]
fn an_older_event_is_ignored() {
    let (po, grn, inv) = ids();
    let first = EdgeKey::new("grn.po", node("grn", grn), node("po", po));
    let second = EdgeKey::new("invoice.grn", node("invoice", inv), node("grn", grn));
    let mut index = GraphIndex::new(support::graph_key());
    index.apply(&add(1, &first)).unwrap();
    index.apply(&add(2, &second)).unwrap();
    assert_eq!(index.edge_count(), 2);

    // A reordered redelivery of the first event is older than the watermark: ignored.
    assert_eq!(index.apply(&add(1, &first)).unwrap(), Applied::Ignored);
    assert_eq!(index.edge_count(), 2);
}

#[test]
fn an_event_for_another_graph_is_rejected() {
    let (po, grn, _) = ids();
    let key = EdgeKey::new("grn.po", node("grn", grn), node("po", po));
    let mut index = GraphIndex::new(support::graph_key());
    let mut event = add(1, &key);
    event.graph = GraphKey::new("other", "hierarchies");

    let error = index.apply(&event).unwrap_err();
    assert_eq!(error.code(), spot_graph_index::WRONG_GRAPH);
    assert_eq!(index.watermark(), 0);
    assert!(!index.is_stale());
}

#[test]
fn an_empty_edge_type_is_invalid() {
    let (po, grn, _) = ids();
    let key = EdgeKey::new("", node("grn", grn), node("po", po));
    let mut index = GraphIndex::new(support::graph_key());
    let error = index.apply(&add(1, &key)).unwrap_err();
    assert_eq!(error.code(), spot_graph_index::INVALID_EDGE);
}

#[test]
fn neighbours_follow_storage_orientation() {
    let (po, grn, inv) = ids();
    let mut index = GraphIndex::new(support::graph_key());
    let grn_po = EdgeKey::new("grn.po", node("grn", grn), node("po", po));
    let inv_grn = EdgeKey::new("invoice.grn", node("invoice", inv), node("grn", grn));
    index.apply(&add(1, &grn_po)).unwrap();
    index.apply(&add(2, &inv_grn)).unwrap();

    let grn_node: NodeRef = node("grn", grn);
    // Outgoing from the receipt reaches the purchase order it holds.
    assert_eq!(
        index.neighbours(&grn_node, "grn.po", Direction::Outgoing),
        vec![node("po", po)]
    );
    // Incoming to the receipt reaches the invoice that points at it.
    assert_eq!(
        index.neighbours(&grn_node, "invoice.grn", Direction::Incoming),
        vec![node("invoice", inv)]
    );
    // Both ways over both edge types.
    let mut both = index.neighbours(&grn_node, "grn.po", Direction::Both);
    both.extend(index.neighbours(&grn_node, "invoice.grn", Direction::Both));
    both.sort();
    assert_eq!(both, vec![node("invoice", inv), node("po", po)]);
    // A record that is not in the graph has no neighbours.
    assert!(
        index
            .neighbours(&node("payment", po), "grn.po", Direction::Both)
            .is_empty()
    );
}

#[test]
fn a_self_edge_never_returns_its_node() {
    let (po, _, _) = ids();
    let key = EdgeKey::new("po.superseded_by", node("po", po), node("po", po));
    let mut index = GraphIndex::new(support::graph_key());
    index.apply(&add(1, &key)).unwrap();
    assert!(
        index
            .neighbours(&node("po", po), "po.superseded_by", Direction::Both)
            .is_empty()
    );
}

#[test]
fn accounted_bytes_match_a_recomputation() {
    let (po, grn, inv) = ids();
    let mut index = GraphIndex::new(support::graph_key());
    index
        .apply(&add(
            1,
            &EdgeKey::new("grn.po", node("grn", grn), node("po", po)),
        ))
        .unwrap();
    index
        .apply(&add(
            2,
            &EdgeKey::new("invoice.grn", node("invoice", inv), node("grn", grn)),
        ))
        .unwrap();
    assert_eq!(index.estimated_bytes(), index.recompute_bytes());
    let before = index.estimated_bytes();

    index
        .apply(&remove(
            3,
            &EdgeKey::new("invoice.grn", node("invoice", inv), node("grn", grn)),
        ))
        .unwrap();
    assert_eq!(index.estimated_bytes(), index.recompute_bytes());
    assert!(index.estimated_bytes() < before);

    index
        .apply(&update(
            4,
            &EdgeKey::new("grn.po", node("grn", grn), node("po", po)),
            &[("note", Value::Text("long enough to matter".to_owned()))],
        ))
        .unwrap();
    assert_eq!(index.estimated_bytes(), index.recompute_bytes());
    assert!(index.estimated_bytes() > 0);
}

#[test]
fn build_collapses_duplicate_edges() {
    let (po, grn, _) = ids();
    let key = EdgeKey::new("grn.po", node("grn", grn), node("po", po));
    let edges = vec![
        spot_graph_index::Edge::bare(key.clone()),
        spot_graph_index::Edge::bare(key.clone()),
    ];
    let index = GraphIndex::build(support::graph_key(), 7, edges).unwrap();
    assert_eq!(index.edge_count(), 1);
    assert_eq!(index.watermark(), 7);
    assert!(!index.is_stale());
    assert_eq!(index.estimated_bytes(), index.recompute_bytes());
}
