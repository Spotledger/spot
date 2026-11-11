//! Strongly connected components: iterative Tarjan with explicit frames
//! ([`strongly_connected_components`]) and the condensation ([`condensation`]).

use crate::error::GraphError;
use crate::limits::{Limits, ensure_node_count, ensure_visited};
use crate::traversal::ensure_node_in_range;
use crate::view::{AdjacencyGraph, GraphView, NodeIx, Weight};

/// The `condensation` half of the deliverable: the condensation graph plus
/// each original node's component id.
type CondensationParts = (AdjacencyGraph, Vec<usize>);

/// One Tarjan frame: the node, its materialised (ascending, deduplicated)
/// successor list, and the next successor slot to scan.
type TarjanFrame = (NodeIx, Vec<(NodeIx, Weight)>, usize);

/// Every strongly connected component, as the brief pins the contract:
///
/// * each component's node list is sorted ascending;
/// * components appear in **reverse topological order of the condensation**:
///   every component appears *after* all components it reaches (sinks
///   first). This is Tarjan's native emission order, stated here as the
///   contract so callers can rely on it and asserted against petgraph in
///   the property tests.
///
/// Determinism: components are sorted sets emitted by an index-driven
/// iterative Tarjan, so the output depends only on the edge multiset, never
/// on the insertion order or the `successors` iteration order.
///
/// # Errors
///
/// * [`GraphError::LimitExceeded`] (kind `Nodes`): `graph.node_count()`
///   exceeds `limits.max_nodes`;
/// * [`GraphError::NodeOutOfRange`]: an edge endpoint outside the graph;
/// * [`GraphError::LimitExceeded`] (kind `Visited`): more than
///   `limits.max_visited` distinct nodes discovered.
#[must_use = "the caller wants the components"]
pub fn strongly_connected_components<G: GraphView>(
    graph: &G,
    limits: &Limits,
) -> Result<Vec<Vec<NodeIx>>, GraphError> {
    compute(graph, limits).map(|(components, _)| components)
}

/// Components only, for the witness search in [`crate::traversal`].
pub(crate) fn compute<G: GraphView>(
    graph: &G,
    limits: &Limits,
) -> Result<(Vec<Vec<NodeIx>>, CondensationParts), GraphError> {
    scc_and_condensation(graph, limits)
}

/// The condensation DAG of `graph`: one node per strongly connected
/// component.
///
/// Returns:
/// 1. an owned [`AdjacencyGraph`] whose node `c` is the `c`-th component of
///    [`strongly_connected_components`] (same emission order), with a
///    deduplicated edge `c -> d` (weight [`crate::UNIT`]) when an edge of
///    the original graph runs from a member of `c` to a member of `d != c`;
/// 2. `component_of`: for every original node (ascending), its component.
///
/// Determinism: the component numbering is deterministic (see
/// [`strongly_connected_components`]) and edges are sorted and
/// deduplicated, so the condensation depends only on the edge multiset.
///
/// # Errors
///
/// Same contract as [`strongly_connected_components`].
#[must_use = "the caller wants the condensation"]
pub fn condensation<G: GraphView>(
    graph: &G,
    limits: &Limits,
) -> Result<CondensationParts, GraphError> {
    scc_and_condensation(graph, limits).map(|(_, parts)| parts)
}

/// One Tarjan pass filling both deliverables.
///
/// The recursion is flattened with an explicit frame stack: a frame
/// materialises its node's successors into a owned scratch `Vec` (so the
/// graph can be borrowed immutably throughout, and the scan position
/// survives across frames), and a tree edge "recurses" by pushing a new
/// frame; when a frame finishes, its low-link folds into the parent's.
fn scc_and_condensation<G: GraphView>(
    graph: &G,
    limits: &Limits,
) -> Result<(Vec<Vec<NodeIx>>, CondensationParts), GraphError> {
    let node_count = ensure_node_count(graph.node_count(), limits)?;
    let mut edges_scratch: Vec<(NodeIx, u64)> = Vec::new();
    for source in 0..node_count {
        edges_scratch.clear();
        edges_scratch.extend(graph.successors(source as NodeIx));
        for &(target, _) in &edges_scratch {
            ensure_node_in_range(graph, target)?;
        }
    }

    let mut component_index: Vec<usize> = vec![usize::MAX; node_count];
    let mut on_stack = vec![false; node_count];
    let mut indices = vec![u32::MAX; node_count];
    let mut low_links = vec![0u32; node_count];
    let mut vertex_stack: Vec<NodeIx> = Vec::new();
    let mut next_index: u32 = 0;
    let mut expanded = 0usize;
    let mut components: Vec<Vec<NodeIx>> = Vec::new();

    for root in 0..node_count {
        if indices[root] != u32::MAX {
            continue;
        }
        // Frame: (node, materialised successors, next slot to scan).
        let mut frames: Vec<TarjanFrame> = Vec::new();
        edges_scratch.clear();
        edges_scratch.extend(graph.successors(root as NodeIx));
        edges_scratch.sort_unstable();
        edges_scratch.dedup();
        frames.push((root as NodeIx, edges_scratch.clone(), 0));
        indices[root] = next_index;
        low_links[root] = next_index;
        next_index += 1;
        vertex_stack.push(root as NodeIx);
        on_stack[root] = true;
        expanded += 1;
        ensure_visited(expanded, limits)?;

        while let Some(frame) = frames.last_mut() {
            let node: NodeIx = frame.0;
            let slot: usize = frame.2;
            if slot < frame.1.len() {
                let target = frame.1[slot].0;
                frame.2 += 1;
                if indices[target as usize] == u32::MAX {
                    // Tree edge: "recurse".
                    indices[target as usize] = next_index;
                    low_links[target as usize] = next_index;
                    next_index += 1;
                    vertex_stack.push(target);
                    on_stack[target as usize] = true;
                    expanded += 1;
                    ensure_visited(expanded, limits)?;
                    edges_scratch.clear();
                    edges_scratch.extend(graph.successors(target));
                    edges_scratch.sort_unstable();
                    edges_scratch.dedup();
                    frames.push((target, edges_scratch.clone(), 0));
                } else if on_stack[target as usize] {
                    // Back edge inside the current tree: low-link update.
                    low_links[node as usize] =
                        low_links[node as usize].min(indices[target as usize]);
                }
                // Cross edge to a settled component: nothing to do.
            } else {
                // All successors scanned: "return" from this frame.
                frames.pop();
                if let Some(parent) = frames.last_mut() {
                    low_links[parent.0 as usize] =
                        low_links[parent.0 as usize].min(low_links[node as usize]);
                }
                if low_links[node as usize] == indices[node as usize] {
                    let mut component: Vec<NodeIx> = Vec::new();
                    while let Some(member) = vertex_stack.pop() {
                        on_stack[member as usize] = false;
                        component.push(member);
                        if member == node {
                            break;
                        }
                    }
                    component.sort_unstable();
                    for &member in &component {
                        component_index[member as usize] = components.len();
                    }
                    components.push(component);
                }
            }
        }
    }

    // ---- Condensation over the component numbering ----
    let mut condensation_adjacency: Vec<Vec<(NodeIx, u64)>> = vec![Vec::new(); components.len()];
    let mut seen_pairs: std::collections::BTreeSet<(usize, usize)> =
        std::collections::BTreeSet::new();
    for source in 0..node_count {
        edges_scratch.clear();
        edges_scratch.extend(graph.successors(source as NodeIx));
        for &(target, _) in &edges_scratch {
            let from_component = component_index[source];
            let to_component = component_index[target as usize];
            if from_component != to_component && seen_pairs.insert((from_component, to_component)) {
                condensation_adjacency[from_component].push((to_component as NodeIx, crate::UNIT));
            }
        }
    }
    for row in &mut condensation_adjacency {
        row.sort_unstable();
    }
    Ok((
        components,
        (
            AdjacencyGraph::from_adjacency(condensation_adjacency),
            component_index,
        ),
    ))
}
