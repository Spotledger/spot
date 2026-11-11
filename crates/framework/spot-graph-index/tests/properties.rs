//! The invariant, as a property: random add/remove/update sequences, with duplicate and reordered
//! redelivery and random restarts, end with the index equal to its source.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;

use proptest::prelude::*;
use spot_graph_index::{
    Applied, Edge, EdgeEvent, EdgeKey, EdgeOp, GraphIndex, GraphKey, IndexStore, MemoryBudget,
    check_equal,
};
use spot_storage_contract::{RecordId, Value};
use support::{MapGraphSource, graph_key, node};

/// The small universe of edges the property test draws from.
fn universe() -> Vec<EdgeKey> {
    let po = RecordId::new_v7();
    let grn = RecordId::new_v7();
    let inv = RecordId::new_v7();
    let pay = RecordId::new_v7();
    vec![
        EdgeKey::new("grn.po", node("grn", grn), node("po", po)),
        EdgeKey::new("invoice.grn", node("invoice", inv), node("grn", grn)),
        EdgeKey::new(
            "payment.invoice",
            node("payment", pay),
            node("invoice", inv),
        ),
        EdgeKey::new("po.superseded_by", node("po", po), node("po", po)),
    ]
}

fn op_of(kind: u8) -> EdgeOp {
    match kind % 3 {
        0 => EdgeOp::Add,
        1 => EdgeOp::Remove,
        _ => EdgeOp::Update,
    }
}

fn props_of(variant: u8) -> BTreeMap<String, Value> {
    let mut props = BTreeMap::new();
    match variant % 3 {
        0 => {}
        1 => {
            props.insert("qty".to_owned(), Value::Int(5));
        }
        _ => {
            props.insert("note".to_owned(), Value::Text("seen".to_owned()));
        }
    }
    props
}

fn apply_to_db(
    db: &mut BTreeMap<EdgeKey, BTreeMap<String, Value>>,
    op: EdgeOp,
    edge: &EdgeKey,
    props: &BTreeMap<String, Value>,
) {
    match op {
        EdgeOp::Add | EdgeOp::Update => {
            db.insert(edge.clone(), props.clone());
        }
        EdgeOp::Remove => {
            db.remove(edge);
        }
    }
}

fn source_of(
    key: &GraphKey,
    db: &BTreeMap<EdgeKey, BTreeMap<String, Value>>,
    watermark: u64,
) -> MapGraphSource {
    MapGraphSource {
        key: key.clone(),
        edges: db
            .iter()
            .map(|(edge, props)| Edge {
                key: edge.clone(),
                properties: props.clone(),
            })
            .collect(),
        watermark,
    }
}

fn edge_at(edges: &[EdgeKey], index: usize) -> EdgeKey {
    edges[index % edges.len()].clone()
}

proptest! {
    #[test]
    fn random_sequences_end_equal_to_the_source(
        ops in prop::collection::vec((0u8..3u8, 0usize..4usize, 0u8..3u8), 0..30),
        restart_at in prop::option::of(0usize..30usize),
        replays in prop::collection::vec(prop::option::of(0usize..30usize), 0..30),
    ) {
        let key = graph_key();
        let edges = universe();
        let mut store = IndexStore::new(MemoryBudget::new(u64::MAX));
        store.insert(GraphIndex::new(key.clone()), 0).unwrap();
        let mut db: BTreeMap<EdgeKey, BTreeMap<String, Value>> = BTreeMap::new();
        let mut delivered: Vec<EdgeEvent> = Vec::new();

        for (i, (kind, edge_index, prop_variant)) in ops.iter().enumerate() {
            let seq = i as u64 + 1;
            let edge = edge_at(&edges, *edge_index);
            let op = op_of(*kind);
            let props = props_of(*prop_variant);
            apply_to_db(&mut db, op, &edge, &props);

            let event = EdgeEvent {
                sequence: seq,
                graph: key.clone(),
                op,
                edge: edge.clone(),
                properties: props,
            };
            store.apply(&event, seq).unwrap();
            delivered.push(event);

            // A reordered redelivery of an already-applied event is ignored and changes nothing.
            if let Some(Some(previous)) = replays.get(i)
                && *previous < delivered.len()
            {
                let applied = store.apply(&delivered[*previous], seq).unwrap();
                prop_assert_eq!(applied, Applied::Ignored);
            }

            // A random restart rebuilds the graph off to the side from the database's state.
            if restart_at == Some(i) {
                let source = source_of(&key, &db, seq);
                let rebuilt = store.rebuild(&key, &source, seq).unwrap();
                prop_assert_eq!(rebuilt.watermark(), seq);
                prop_assert!(!rebuilt.is_stale());
            }
        }

        let index = store.peek(&key).unwrap();
        prop_assert_eq!(index.watermark(), ops.len() as u64);
        prop_assert!(!index.is_stale());
        let source = source_of(&key, &db, ops.len() as u64);
        let drifts = check_equal(index, &source).unwrap();
        prop_assert!(drifts.is_empty(), "drift: {drifts:?}");
    }
}

#[test]
fn a_gap_marks_stale_and_a_rebuild_repairs_it() {
    let key = graph_key();
    let edges = universe();
    let mut store = IndexStore::new(MemoryBudget::new(u64::MAX));

    let first = EdgeEvent::bare(1, key.clone(), EdgeOp::Add, edges[0].clone());
    store.apply(&first, 1).unwrap();

    // Skip sequence 2: the graph is marked stale and refuses to guess.
    let third = EdgeEvent::bare(3, key.clone(), EdgeOp::Add, edges[1].clone());
    assert_eq!(
        store.apply(&third, 2).unwrap_err().code(),
        spot_graph_index::GAP
    );
    assert!(store.peek(&key).unwrap().is_stale());

    // Rebuild from the database's state at watermark 3; the graph is fresh and equal.
    let mut db: BTreeMap<EdgeKey, BTreeMap<String, Value>> = BTreeMap::new();
    db.insert(edges[0].clone(), BTreeMap::new());
    db.insert(edges[1].clone(), BTreeMap::new());
    let source = source_of(&key, &db, 3);
    let rebuilt = store.rebuild(&key, &source, 3).unwrap();
    assert!(!rebuilt.is_stale());
    assert!(check_equal(&rebuilt, &source).unwrap().is_empty());
}

#[test]
fn a_behind_index_is_reported_as_drift() {
    let key = graph_key();
    let edges = universe();
    let mut db: BTreeMap<EdgeKey, BTreeMap<String, Value>> = BTreeMap::new();
    db.insert(edges[0].clone(), BTreeMap::new());
    let source = source_of(&key, &db, 1);
    // An empty index against a source with one edge is drift, so the checker is not vacuous.
    let index = GraphIndex::new(key.clone());
    let drifts = check_equal(&index, &source).unwrap();
    assert!(!drifts.is_empty());
}
