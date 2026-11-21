//! Brief §4 determinism: the output is identical across 100 shuffled edge
//! insertion orders — and also across `successors` iteration orders (a
//! misbehaving-in-order `GraphView` wrapper), because the algorithms' scan
//! order is canonical, never the graph's.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (house pattern of the other framework crates' integration tests).
#![allow(clippy::unwrap_used)]

use spot_graph_algo::{
    AdjacencyGraph, GraphView, Limits, NodeIx, UNIT, Weight, condensation, distance_levels,
    find_cycle, reachable, reversed, shortest_path, strongly_connected_components,
    topological_order,
};

/// A deterministic xorshift shuffle (test-only; the algorithms never see
/// randomness).
fn shuffled<T>(mut items: Vec<T>, seed: u64) -> Vec<T> {
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for i in (1..items.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
    items
}

/// A `GraphView` that yields a node's successors in a seed-shuffled order —
/// proving the outputs depend on the edge multiset, never on iteration order.
struct ShuffledView {
    inner: AdjacencyGraph,
    order: Vec<Vec<NodeIx>>,
}

impl ShuffledView {
    fn wrap(graph: &AdjacencyGraph, seed: u64) -> Self {
        let order = (0..graph.node_count() as NodeIx)
            .map(|node| {
                shuffled(
                    graph.successors(node).map(|(t, _)| t).collect(),
                    seed + u64::from(node),
                )
            })
            .collect();
        Self {
            inner: graph.clone(),
            order,
        }
    }
}

impl GraphView for ShuffledView {
    fn node_count(&self) -> usize {
        self.inner.node_count()
    }
    fn successors(&self, node: NodeIx) -> impl Iterator<Item = (NodeIx, Weight)> {
        let inner = &self.inner;
        self.order[node as usize]
            .clone()
            .into_iter()
            .map(move |target| {
                // Recover the (first) weight of that edge.
                inner
                    .successors(node)
                    .find(|&(t, _)| t == target)
                    .unwrap_or((target, UNIT))
            })
    }
}

/// A graph with every shape the brief lists: chains, cross edges, a diamond,
/// parallel edges, self loop, disconnected part and mixed weights.
fn many_shapes() -> Vec<(&'static str, AdjacencyGraph)> {
    let mut shapes: Vec<(&'static str, AdjacencyGraph)> = Vec::new();
    // A DAG with cross edges (10 nodes).
    let mut dag = AdjacencyGraph::new(10).unwrap();
    for i in 0..9 {
        dag.add_unit_edge(i, i + 1).unwrap();
    }
    for &(a, b) in &[(0, 3), (1, 4), (2, 6), (3, 7), (0, 5), (5, 8)] {
        dag.add_unit_edge(a, b).unwrap();
    }
    shapes.push(("dag", dag));
    // A weighted DAG (ties everywhere: every route cost-parity).
    let mut weighted = AdjacencyGraph::new(6).unwrap();
    weighted.add_edge(0, 1, 2).unwrap();
    weighted.add_edge(0, 2, 2).unwrap();
    weighted.add_edge(1, 3, 1).unwrap();
    weighted.add_edge(2, 3, 1).unwrap();
    weighted.add_edge(1, 4, 3).unwrap();
    weighted.add_edge(4, 5, 1).unwrap();
    weighted.add_edge(3, 5, 3).unwrap();
    shapes.push(("weighted", weighted));
    // Cyclic: a 3-cycle on 1..3, a 2-cycle on 6..7, isolated 8, pendant 4.
    let cyclic = AdjacencyGraph::from_edges(
        9,
        &[
            (0, 1),
            (1, 2),
            (2, 1),
            (1, 3),
            (3, 1),
            (6, 7),
            (7, 6),
            (2, 4),
        ],
    )
    .unwrap();
    shapes.push(("cyclic", cyclic));
    // Self loop + parallel edges + disconnected parts.
    let mut mixed = AdjacencyGraph::new(7).unwrap();
    mixed.add_unit_edge(0, 1).unwrap();
    mixed.add_unit_edge(0, 1).unwrap();
    mixed.add_unit_edge(1, 1).unwrap();
    mixed.add_unit_edge(4, 5).unwrap();
    mixed.add_unit_edge(5, 6).unwrap();
    shapes.push(("mixed", mixed));
    shapes
}

/// The full result tuple of every algorithm over `graph` — every component
/// is an owned output that must not depend on insertion order.
fn snapshot<G: GraphView>(graph: &G) -> Snapshot {
    let limits = Limits::default();
    Snapshot {
        topo: topological_order(graph, &limits).ok(),
        cycle: find_cycle(graph, &limits).ok().flatten(),
        reachable: reachable(graph, &[0, 6], None, &limits).ok(),
        levels: distance_levels(graph, &[0, 6], None, &limits).ok(),
        path: shortest_path(graph, 0, 8, &limits).ok().flatten(),
        scc: strongly_connected_components(graph, &limits).ok(),
        condensation: condensation(graph, &limits).ok(),
        reverse_topo: topological_order(&reversed(graph), &limits).ok(),
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct Snapshot {
    topo: Option<Vec<NodeIx>>,
    cycle: Option<spot_graph_algo::Cycle>,
    reachable: Option<Vec<NodeIx>>,
    levels: Option<Vec<Vec<NodeIx>>>,
    path: Option<spot_graph_algo::Path>,
    scc: Option<Vec<Vec<NodeIx>>>,
    condensation: Option<(AdjacencyGraph, Vec<usize>)>,
    reverse_topo: Option<Vec<NodeIx>>,
}

#[test]
fn outputs_are_identical_across_100_shuffled_insertion_orders() {
    for (name, graph) in many_shapes() {
        let edges: Vec<(NodeIx, NodeIx, Weight)> = (0..graph.node_count() as NodeIx)
            .flat_map(|node| graph.successors(node).map(move |(t, w)| (node, t, w)))
            .collect();
        let reference = snapshot(&graph);
        // 100 seeds: 0..100 (the unshuffled reference is the first).
        for seed in 0..100u64 {
            let shuffled_edges = shuffled(edges.clone(), seed);
            let node_count = graph.node_count();
            let candidate = snapshot(
                &AdjacencyGraph::from_weighted_edges(node_count, &shuffled_edges).unwrap(),
            );
            assert_eq!(candidate.topo, reference.topo, "{name} topo, seed {seed}");
            assert_eq!(
                candidate.cycle, reference.cycle,
                "{name} cycle, seed {seed}"
            );
            assert_eq!(
                candidate.reachable, reference.reachable,
                "{name} reachable, seed {seed}"
            );
            assert_eq!(
                candidate.levels, reference.levels,
                "{name} levels, seed {seed}"
            );
            assert_eq!(candidate.path, reference.path, "{name} path, seed {seed}");
            assert_eq!(candidate.scc, reference.scc, "{name} scc, seed {seed}");
            assert_eq!(
                candidate.condensation, reference.condensation,
                "{name} condensation, seed {seed}"
            );
            assert_eq!(
                candidate.reverse_topo, reference.reverse_topo,
                "{name} reversed topo, seed {seed}"
            );
        }
    }
}

#[test]
fn outputs_are_identical_across_successors_iteration_orders() {
    for (name, graph) in many_shapes() {
        let reference = snapshot(&graph);
        for seed in 0..25u64 {
            let view = ShuffledView::wrap(&graph, seed);
            let candidate = snapshot(&view);
            assert_eq!(candidate, reference, "{name}, view seed {seed}");
        }
    }
}

#[test]
fn the_shuffled_dag_twin_is_lexicographically_identical() {
    // Like the spot-dag property: a chain-plus-cross-edges DAG and its
    // shuffled twin must share the exact topological order.
    let node_count = 12u32;
    let mut edges: Vec<(NodeIx, NodeIx)> = Vec::new();
    for i in 0..node_count - 1 {
        edges.push((i, i + 1));
    }
    for &(a, b) in &[(0, 5), (1, 7), (2, 9), (3, 4), (5, 11)] {
        edges.push((a, b));
    }
    let reference = topological_order(
        &AdjacencyGraph::from_edges(node_count as usize, &edges).unwrap(),
        &Limits::default(),
    )
    .unwrap();
    for seed in 0..100u64 {
        let twin = shuffled(edges.clone(), seed);
        let candidate = topological_order(
            &AdjacencyGraph::from_edges(node_count as usize, &twin).unwrap(),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(candidate, reference, "seed {seed}");
    }
}
