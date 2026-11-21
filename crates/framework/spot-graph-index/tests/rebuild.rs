//! The rebuild path and the invariant checker: a fresh index equals its source, every kind of
//! drift is reported, and a rebuild never exposes a half-built graph.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_graph_index::{
    Drift, Edge, EdgeKey, GraphIndex, GraphKey, IndexStore, MemoryBudget, check_equal,
};
use spot_storage_contract::Value;
use support::{Chain, RecordSource, graph_key, graph_model, node};

fn index_and_source(chain: &Chain) -> (GraphIndex, RecordSource) {
    let model = graph_model();
    let records = chain.records();
    let edges = support::edges_of(&model, &records);
    let index = GraphIndex::build(graph_key(), 3, edges).unwrap();
    let source = RecordSource {
        model,
        records,
        watermark: 3,
    };
    (index, source)
}

#[test]
fn a_fresh_index_equals_its_source() {
    let chain = Chain::new();
    let (index, source) = index_and_source(&chain);
    assert!(check_equal(&index, &source).unwrap().is_empty());
    assert!(index.edge_count() > 0);
}

#[test]
fn a_missing_edge_is_drift() {
    let chain = Chain::new();
    let (mut index, source) = index_and_source(&chain);
    let removed = EdgeKey::new("grn.po", node("grn", chain.g1), node("po", chain.p1));
    // Remove the edge directly through an event so the index is missing one edge the source has.
    let event = spot_graph_index::EdgeEvent::bare(
        4,
        graph_key(),
        spot_graph_index::EdgeOp::Remove,
        removed.clone(),
    );
    index.apply(&event).unwrap();

    let drifts = check_equal(&index, &source).unwrap();
    assert!(
        drifts
            .iter()
            .any(|d| matches!(d, Drift::MissingEdge(edge) if *edge == removed)),
        "{drifts:?}"
    );
}

#[test]
fn an_extra_edge_is_drift() {
    let chain = Chain::new();
    let model = graph_model();
    let records = chain.records();
    let mut edges = support::edges_of(&model, &records);
    let extra = EdgeKey::new(
        "po.superseded_by",
        node("po", chain.p1),
        node("po", chain.p1),
    );
    edges.push(Edge::bare(extra.clone()));
    let index = GraphIndex::build(graph_key(), 3, edges).unwrap();
    let source = RecordSource {
        model,
        records,
        watermark: 3,
    };
    let drifts = check_equal(&index, &source).unwrap();
    assert!(
        drifts
            .iter()
            .any(|d| matches!(d, Drift::ExtraEdge(edge) if *edge == extra)),
        "{drifts:?}"
    );
}

#[test]
fn a_property_mismatch_is_drift() {
    let chain = Chain::new();
    let key = EdgeKey::new("grn.po", node("grn", chain.g1), node("po", chain.p1));
    let mut properties = BTreeMap::new();
    properties.insert("qty".to_owned(), Value::Int(5));
    let index = GraphIndex::build(
        graph_key(),
        3,
        vec![Edge {
            key: key.clone(),
            properties,
        }],
    )
    .unwrap();
    let source = support::MapGraphSource {
        key: graph_key(),
        edges: vec![Edge::bare(key.clone())],
        watermark: 3,
    };
    let drifts = check_equal(&index, &source).unwrap();
    assert!(
        drifts.iter().any(|d| matches!(
            d,
            Drift::PropertyMismatch { edge, .. } if *edge == key
        )),
        "{drifts:?}"
    );
}

#[test]
fn a_watermark_mismatch_is_drift() {
    let chain = Chain::new();
    let (index, _) = index_and_source(&chain);
    let behind = support::MapGraphSource {
        key: graph_key(),
        edges: index.edges().collect(),
        watermark: 9,
    };
    let drifts = check_equal(&index, &behind).unwrap();
    assert_eq!(
        drifts,
        vec![Drift::WatermarkMismatch {
            index: 3,
            source: 9
        }]
    );
}

#[test]
fn a_rebuild_leaves_the_old_graph_intact_for_a_reader() {
    let chain = Chain::new();
    let model = graph_model();
    let records = chain.records();
    let partial = Arc::new(
        GraphIndex::build(
            graph_key(),
            1,
            vec![Edge::bare(EdgeKey::new(
                "grn.po",
                node("grn", chain.g1),
                node("po", chain.p1),
            ))],
        )
        .unwrap(),
    );
    let mut store = IndexStore::new(MemoryBudget::new(1_000_000));
    store.insert((*partial).clone(), 1).unwrap();

    // A reader captured the old graph; a rebuild swaps in a complete one.
    let reader = partial.clone();
    let source = RecordSource {
        model,
        records,
        watermark: 3,
    };
    store.rebuild(&graph_key(), &source, 2).unwrap();

    assert_eq!(reader.edge_count(), 1, "the old graph is unchanged");
    assert!(!reader.is_stale());
    let current = store.peek(&graph_key()).unwrap();
    assert_eq!(current.edge_count(), 4);
    assert_eq!(current.watermark(), 3);
    assert_eq!(store.used_bytes(), current.estimated_bytes());
    assert_eq!(store.used_bytes(), current.recompute_bytes());
}

#[test]
fn a_source_that_reports_an_unknown_graph_is_asked_for_the_indexs_key() {
    // The source is asked for the index's own key, not a caller-supplied one.
    struct Echo {
        seen: std::cell::RefCell<Vec<GraphKey>>,
    }
    impl spot_graph_index::GraphSource for Echo {
        fn stream_edges(
            &self,
            key: &GraphKey,
        ) -> Result<Vec<Edge>, spot_graph_index::GraphIndexError> {
            self.seen.borrow_mut().push(key.clone());
            Ok(vec![Edge::bare(EdgeKey::new(
                "grn.po",
                node("grn", spot_storage_contract::RecordId::new_v7()),
                node("po", spot_storage_contract::RecordId::new_v7()),
            ))])
        }
        fn watermark(&self, key: &GraphKey) -> Result<u64, spot_graph_index::GraphIndexError> {
            self.seen.borrow_mut().push(key.clone());
            Ok(1)
        }
    }
    let source = Echo {
        seen: std::cell::RefCell::new(Vec::new()),
    };
    let index = GraphIndex::new(graph_key());
    let _ = check_equal(&index, &source).unwrap();
    assert_eq!(source.seen.borrow().as_slice(), &[graph_key(), graph_key()]);
}
