//! Property tests (brief §4): for random graphs up to 200 nodes, the
//! algorithms of `spot-graph-algo` agree with `petgraph` — the dev-only
//! reference oracle — on reachability, SCC membership, cycle existence and
//! shortest-path cost.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (house pattern of the other framework crates' integration tests).
#![allow(clippy::unwrap_used)]

use petgraph::algo::{dijkstra, has_path_connecting, kosaraju_scc};
use petgraph::prelude::*;
use proptest::prelude::*;
use spot_graph_algo::{
    AdjacencyGraph, GraphView, Limits, NodeIx, UNIT, find_cycle, reachable, shortest_path,
    strongly_connected_components, topological_order,
};

/// Builds both representations from the same edge list.
struct Pair {
    edges: Vec<(NodeIx, NodeIx, u64)>,
    ours: AdjacencyGraph,
    theirs: DiGraph<u32, u64>,
}

/// Edges only from a lower to a higher index (`dag_gen`): acyclic by
/// construction, so `topological_order`, Dijkstra and reachability all have
/// their well-defined meaning on every sample.
fn dag_gen(node_count: u32, edge_seed: u64) -> Vec<(u32, u32, u64)> {
    let mut state = edge_seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut edges = Vec::new();
    for from in 0..node_count.saturating_sub(1) {
        // Chain edge keeps everything connected; two random cross edges per
        // node keep the shapes varied.
        edges.push((from, from + 1, 1 + next() % 5));
        for _ in 0..2 {
            let to = from + 1 + (next() % u64::from(node_count - from - 1)) as u32;
            edges.push((from, to, 1 + next() % 9));
        }
    }
    edges
}

/// General random digraph (`digraph_gen`): parallel edges, self loops,
/// disconnected parts, cycles — everything the brief asks the properties to
/// survive.
fn digraph_gen(node_count: u32, edge_seed: u64) -> Vec<(u32, u32, u64)> {
    let mut state = edge_seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut edges = Vec::new();
    for _ in 0..node_count * 2 {
        let from = (next() % u64::from(node_count)) as u32;
        let to = (next() % u64::from(node_count)) as u32;
        edges.push((from, to, next() % 7 + 1));
    }
    edges
}

fn build(node_count: usize, edges: &[(u32, u32, u64)]) -> Pair {
    let ours = AdjacencyGraph::from_weighted_edges(node_count, edges).unwrap();
    let mut theirs = DiGraph::new();
    let nodes: Vec<NodeIndex> = (0..node_count).map(|i| theirs.add_node(i as u32)).collect();
    for &(from, to, weight) in edges {
        theirs.add_edge(nodes[from as usize], nodes[to as usize], weight);
    }
    Pair {
        edges: edges.to_vec(),
        ours,
        theirs,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Reachability agrees with petgraph on random DAGs (single source).
    #[test]
    fn reachability_matches_petgraph(
        node_count in 1u32..=200,
        edge_seed in any::<u64>(),
    ) {
        let pair = build(node_count as usize, &dag_gen(node_count, edge_seed));
        let limits = Limits::default();
        let ours = reachable(&pair.ours, &[0], None, &limits).unwrap();
        let mut theirs: Vec<u32> = (0..node_count)
            .filter(|&n| has_path_connecting(&pair.theirs, NodeIndex::new(0), NodeIndex::new(n as usize), None))
            .collect();
        theirs.sort_unstable();
        prop_assert_eq!(ours, theirs);
    }

    /// SCC membership agrees with kosaraju_scc on random digraphs.
    #[test]
    fn scc_membership_matches_petgraph(
        node_count in 1u32..=200,
        edge_seed in any::<u64>(),
    ) {
        let pair = build(node_count as usize, &digraph_gen(node_count, edge_seed));
        let ours = strongly_connected_components(&pair.ours, &Limits::default()).unwrap();
        let theirs = kosaraju_scc(&pair.theirs);
        prop_assert_eq!(ours.len(), theirs.len(), "component count");
        // Membership as a canonical partition (each component sorted, the
        // component list sorted): our emission order (sinks first) is
        // deliberately different from kosaraju's, so only the partition —
        // not the numbering — must agree.
        let canonical = |components: &[Vec<u32>]| -> Vec<Vec<u32>> {
            let mut comps: Vec<Vec<u32>> = components
                .iter()
                .map(|members| {
                    let mut sorted = members.clone();
                    sorted.sort_unstable();
                    sorted
                })
                .collect();
            comps.sort();
            comps
        };
        let ours_canonical = canonical(&ours);
        let theirs_canonical = canonical(
            &theirs
                .iter()
                .map(|members| members.iter().map(|node| node.index() as u32).collect())
                .collect::<Vec<Vec<u32>>>(),
        );
        prop_assert_eq!(ours_canonical, theirs_canonical);
        // Every component sorted ascending (our contract).
        for members in &ours {
            let mut sorted = members.clone();
            sorted.sort_unstable();
            prop_assert_eq!(&sorted, members);
        }
    }

    /// Cycle existence agrees with petgraph (DAG vs cyclic digraph).
    #[test]
    fn cycle_existence_matches_petgraph(
        node_count in 1u32..=200,
        edge_seed in any::<u64>(),
        with_back_edges in 0u32..=3,
    ) {
        let mut edges = dag_gen(node_count, edge_seed);
        // Splice in back edges (higher -> lower) to create cycles.
        let mut state = edge_seed ^ 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..with_back_edges {
            let from = (next() % u64::from(node_count)) as u32;
            let to = (next() % u64::from(node_count)) as u32;
            edges.push((from, to, 1));
        }
        let pair = build(node_count.max(1) as usize, &edges);
        let ours = find_cycle(&pair.ours, &Limits::default()).unwrap().is_some();
        let theirs = petgraph::algo::is_cyclic_directed(&pair.theirs);
        prop_assert_eq!(ours, theirs);
        // And Kahn agrees with both.
        let topo_ok = topological_order(&pair.ours, &Limits::default()).is_ok();
        prop_assert_eq!(topo_ok, !theirs);
    }

    /// Shortest-path cost agrees with petgraph's dijkstra (DAG, node 0).
    #[test]
    fn shortest_path_cost_matches_petgraph(
        node_count in 1u32..=200,
        edge_seed in any::<u64>(),
    ) {
        let pair = build(node_count as usize, &dag_gen(node_count, edge_seed));
        let limits = Limits::default();
        for target in 0..node_count {
            let ours = shortest_path(&pair.ours, 0, target, &limits).unwrap();
            let theirs = dijkstra(&pair.theirs, NodeIndex::new(0), None, |edge| *edge.weight());
            // petgraph's dijkstra map has an entry only for reached nodes.
            let expected = theirs
                .get(&NodeIndex::new(target as usize))
                .copied()
                .unwrap_or(u64::MAX);
            match ours {
                Some(path) => {
                    // petgraph's u64::MAX is its own unreachable sentinel.
                    prop_assert!(expected < u64::MAX, "ours found a path, petgraph did not");
                    prop_assert_eq!(path.cost, expected, "target {}", target);
                    // The path's edges must really sum to the cost: a
                    // parallel (f, t) family resolves to its *minimum*
                    // weight, the one a min-cost path would use.
                    let mut walked: u64 = 0;
                    for window in path.nodes.windows(2) {
                        let edge = pair
                            .edges
                            .iter()
                            .filter(|&&(f, t, _)| f == window[0] && t == window[1])
                            .map(|&(_, _, w)| w)
                            .min();
                        prop_assert!(edge.is_some(), "path edge {:?} missing", window);
                        walked += edge.unwrap();
                    }
                    prop_assert_eq!(walked, path.cost, "cost is the real edge sum");
                    prop_assert_eq!(path.nodes.first(), Some(&0u32));
                    prop_assert_eq!(path.nodes.last(), Some(&target));
                }
                None => prop_assert!(expected == u64::MAX, "petgraph found a path, ours did not (target {})", target),
            }
        }
    }

    /// Unit-weight digraphs: BFS cost equals petgraph BFS cost; also the
    /// max_visited bound triggers exactly where promised.
    #[test]
    fn unit_shortest_path_and_limits(
        node_count in 1u32..=200,
        edge_seed in any::<u64>(),
    ) {
        let edges: Vec<(u32, u32, u64)> = digraph_gen(node_count, edge_seed)
            .into_iter()
            .map(|(f, t, _)| (f, t, UNIT))
            .collect();
        let pair = build(node_count as usize, &edges);
        let limits = Limits::default();
        let targets: Vec<u32> = (0..node_count.min(8)).collect();
        for target in targets {
            let ours = shortest_path(&pair.ours, 0, target, &limits).unwrap();
            let theirs = dijkstra(&pair.theirs, NodeIndex::new(0), None, |edge| *edge.weight());
            let expected = theirs
                .get(&NodeIndex::new(target as usize))
                .copied()
                .unwrap_or(u64::MAX);
            match ours {
                Some(path) => prop_assert_eq!(path.cost, expected, "target {}", target),
                None => prop_assert!(expected == u64::MAX, "target {} unreachable", target),
            }
        }
        // Every node reachable set non-trivially: bounded visited = the
        // reachable count must trigger LimitExceeded when max_visited is
        // smaller.
        let reach = reachable(&pair.ours, &[0], None, &limits).unwrap();
        for cap in 1..=reach.len() {
            let bounded = reachable(&pair.ours, &[0], None, &Limits::bounded_visited(cap));
            if cap < reach.len() {
                prop_assert!(bounded.is_err(), "cap {cap} under {len}", len = reach.len());
            } else {
                prop_assert!(bounded.is_ok(), "cap {cap} at {len}", len = reach.len());
            }
        }
    }

    /// The condensation has exactly one node per SCC and its edges are the
    /// deduplicated inter-component edges (checked against the same
    /// membership petgraph found).
    #[test]
    fn condensation_shape(
        node_count in 1u32..=200,
        edge_seed in any::<u64>(),
    ) {
        let pair = build(node_count as usize, &digraph_gen(node_count, edge_seed));
        let (condensation, component_of) =
            spot_graph_algo::condensation(&pair.ours, &Limits::default()).unwrap();
        let theirs = kosaraju_scc(&pair.theirs);
        prop_assert_eq!(condensation.node_count(), theirs.len());
        // Every condensation edge (c, d) must come from a real inter-component edge.
        for from_component in 0..condensation.node_count() {
            for (to_component, _) in condensation.successors(from_component as u32) {
                prop_assert_ne!(from_component, to_component as usize);
                let has_bridge = pair.edges.iter().any(|&(f, t, _)| {
                    component_of[f as usize] == from_component
                        && component_of[t as usize] == to_component as usize
                });
                prop_assert!(has_bridge, "condensation edge {from_component} -> {to_component} has no original edge");
            }
        }
        // And every real bridge is present.
        for &(f, t, _) in &pair.edges {
            let from = component_of[f as usize];
            let to = component_of[t as usize];
            if from != to {
                let present = condensation
                    .successors(from as u32)
                    .any(|(t, _)| t as usize == to);
                prop_assert!(present, "missing condensation edge {from} -> {to}");
            }
        }
    }
}
