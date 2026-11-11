//! The memory budget: deterministic accounting, least-recently-used eviction of cold graphs, and
//! the pin list.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_graph_index::{
    Applied, Edge, EdgeEvent, EdgeKey, EdgeOp, GraphIndex, GraphKey, GraphSource, IndexStore,
    MemoryBudget,
};
use spot_storage_contract::RecordId;

use support::node;

fn edge(edge_type: &str, from: (&str, RecordId), to: (&str, RecordId)) -> Edge {
    Edge::bare(EdgeKey::new(
        edge_type,
        node(from.0, from.1),
        node(to.0, to.1),
    ))
}

fn graph(tenant: &str, graph: &str, id: RecordId) -> GraphIndex {
    let key = GraphKey::new(tenant, graph);
    let edges = vec![edge("grn.po", ("grn", id), ("po", id))];
    GraphIndex::build(key, 1, edges).unwrap()
}

fn event(key: &GraphKey, seq: u64, edge: EdgeKey) -> EdgeEvent {
    EdgeEvent::bare(seq, key.clone(), EdgeOp::Add, edge)
}

#[test]
fn lru_eviction_drops_the_coldest_unpinned_graph() {
    let id = RecordId::new_v7();
    let a = GraphKey::new("t", "a");
    let b = GraphKey::new("t", "b");
    let c = GraphKey::new("t", "c");
    let d = GraphKey::new("t", "d");
    let one = graph("t", "a", id).estimated_bytes();
    assert!(one > 0);

    let mut store = IndexStore::new(MemoryBudget::new(one * 3));
    store.insert(graph("t", "a", id), 1).unwrap();
    store.insert(graph("t", "b", id), 2).unwrap();
    store.insert(graph("t", "c", id), 3).unwrap();
    assert_eq!(store.len(), 3);
    assert_eq!(store.used_bytes(), one * 3);

    // Touch a and c so b is the coldest.
    let _ = store.get(&a, 4);
    let _ = store.get(&c, 5);
    store.insert(graph("t", "d", id), 6).unwrap();

    assert!(!store.contains(&b), "the coldest graph was evicted");
    assert!(store.contains(&a) && store.contains(&c) && store.contains(&d));
    assert_eq!(store.used_bytes(), one * 3);
}

#[test]
fn a_pinned_graph_is_never_evicted() {
    let id = RecordId::new_v7();
    let a = GraphKey::new("t", "a");
    let b = GraphKey::new("t", "b");
    let c = GraphKey::new("t", "c");
    let d = GraphKey::new("t", "d");
    let one = graph("t", "a", id).estimated_bytes();

    let mut store = IndexStore::new(MemoryBudget::new(one * 3));
    store.insert(graph("t", "a", id), 1).unwrap();
    store.insert(graph("t", "b", id), 2).unwrap();
    store.insert(graph("t", "c", id), 3).unwrap();
    store.pin(&a);
    store.pin(&b);
    store.pin(&c);
    assert!(store.is_pinned(&a));

    let error = store.insert(graph("t", "d", id), 4).unwrap_err();
    assert_eq!(error.code(), spot_graph_index::BUDGET_EXCEEDED);
    assert!(
        store.contains(&d),
        "the graph being written is not the victim"
    );
    assert_eq!(store.len(), 4);

    // Unpinning the coldest lets the cap be met again.
    store.unpin(&b);
    store.enforce_cap(None).unwrap();
    assert!(!store.contains(&b));
    assert_eq!(store.len(), 3);
    assert_eq!(store.used_bytes(), one * 3);
}

#[test]
fn apply_creates_a_graph_and_accounts_it() {
    let key = GraphKey::new("t", "a");
    let id = RecordId::new_v7();
    let mut store = IndexStore::new(MemoryBudget::new(1_000_000));
    let event = event(
        &key,
        1,
        EdgeKey::new("grn.po", node("grn", id), node("po", id)),
    );

    assert_eq!(store.apply(&event, 1).unwrap(), Applied::Added);
    assert!(store.contains(&key));
    let graph = store.peek(&key).unwrap();
    assert_eq!(graph.edge_count(), 1);
    assert_eq!(store.used_bytes(), graph.estimated_bytes());
    assert_eq!(store.used_bytes(), graph.recompute_bytes());
}

#[test]
fn rebuild_swaps_in_a_complete_graph_and_clears_staleness() {
    let key = GraphKey::new("t", "a");
    let id = RecordId::new_v7();
    let mut store = IndexStore::new(MemoryBudget::new(1_000_000));

    // A gap marks the stored graph stale.
    let first = EdgeKey::new("grn.po", node("grn", id), node("po", id));
    store.apply(&event(&key, 1, first), 1).unwrap();
    let gap = event(
        &key,
        3,
        EdgeKey::new("invoice.grn", node("invoice", id), node("grn", id)),
    );
    assert_eq!(
        store.apply(&gap, 2).unwrap_err().code(),
        spot_graph_index::GAP
    );
    assert!(store.peek(&key).unwrap().is_stale());

    // The source holds both edges at watermark 3; a rebuild makes the graph fresh and complete.
    let source = support::MapGraphSource {
        key: key.clone(),
        edges: vec![
            edge("grn.po", ("grn", id), ("po", id)),
            edge("invoice.grn", ("invoice", id), ("grn", id)),
        ],
        watermark: 3,
    };
    let rebuilt = store.rebuild(&key, &source, 3).unwrap();
    assert!(!rebuilt.is_stale());
    assert_eq!(rebuilt.edge_count(), 2);
    assert_eq!(rebuilt.watermark(), 3);
    assert_eq!(store.peek(&key).unwrap().edge_count(), 2);
    assert_eq!(store.used_bytes(), rebuilt.estimated_bytes());

    // The next event is 4: the watermark carried through the rebuild.
    let next = event(
        &key,
        4,
        EdgeKey::new("payment.invoice", node("payment", id), node("invoice", id)),
    );
    assert_eq!(store.apply(&next, 4).unwrap(), Applied::Added);
}

#[test]
fn a_removed_graph_frees_its_bytes() {
    let key = GraphKey::new("t", "a");
    let id = RecordId::new_v7();
    let mut store = IndexStore::new(MemoryBudget::new(1_000_000));
    store.insert(graph("t", "a", id), 1).unwrap();
    assert!(store.used_bytes() > 0);
    assert!(store.remove(&key).is_some());
    assert!(store.is_empty());
    assert_eq!(store.used_bytes(), 0);
}

#[test]
fn a_failing_source_leaves_the_store_untouched() {
    struct Failing;
    impl GraphSource for Failing {
        fn stream_edges(
            &self,
            _key: &GraphKey,
        ) -> Result<Vec<Edge>, spot_graph_index::GraphIndexError> {
            Err(spot_graph_index::GraphIndexError::source(
                "the database is down",
            ))
        }
        fn watermark(&self, _key: &GraphKey) -> Result<u64, spot_graph_index::GraphIndexError> {
            Ok(0)
        }
    }
    let key = GraphKey::new("t", "a");
    let mut store = IndexStore::new(MemoryBudget::new(1_000_000));
    let error = store.rebuild(&key, &Failing, 1).unwrap_err();
    assert_eq!(error.code(), spot_graph_index::SOURCE);
    assert!(store.is_empty());
}
