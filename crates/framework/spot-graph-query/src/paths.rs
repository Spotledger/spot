//! The summed rollup (WP-GRA-03.T3): for each reached node, the sum over every simple path from a
//! start node of the product of one edge property along the path.
//!
//! It runs after the breadth-first walk, over the **visible subgraph** the walk accepted: a path
//! through a hidden or filtered node never exists here, and an edge that fails the edge filter is
//! not followed, so the sum never reveals a hidden node's quantity.
//!
//! Without a cycle the sum is computed per (node, path length): the sum at a node after `k` edges
//! is the sum at each predecessor after `k - 1` edges times that edge's factor, so a diamond-heavy
//! graph costs its edges times its depth, not its number of paths (the number of paths is carried
//! alongside, as a count). With a cycle that cannot be memoised (a walk may not enter a node
//! already on its path) and the simple paths are enumerated one by one. Either way the work is
//! bounded by `max_paths` and exceeding it is `graphq.too_many_paths`, never a partial sum.

use std::collections::{BTreeMap, BTreeSet};

use spot_storage_contract::{NodeRef, Value};

use crate::compile::Step;
use crate::error::GraphQueryError;
use crate::exec::{
    Invocation, Needs, NodeSource, Seen, edge_passes, edge_props_of, exact_sum, rollup_step,
};
use crate::query::GraphQuery;

/// One crossed edge between two accepted nodes.
struct Edge {
    to: NodeRef,
    step: usize,
    factor: Value,
}

type Adjacency = BTreeMap<NodeRef, Vec<Edge>>;

/// The running sum and the number of paths behind it at one node.
type Entry = (Value, u128);

/// Computes the sums and counts and stores them in `seen.rolled` and `seen.counts`.
pub(crate) async fn sum_over_paths<S: NodeSource>(
    source: &mut S,
    steps: &[Step],
    query: &GraphQuery,
    who: &Invocation<'_>,
    seen: &mut Seen,
    needs: &Needs,
) -> Result<(), GraphQueryError> {
    let Some(property) = query.rollup.as_ref() else {
        return Ok(());
    };
    let adjacency = adjacency_of(source, steps, query, who, seen, needs, property).await?;
    let starts: Vec<NodeRef> = seen
        .depth
        .iter()
        .filter(|(_, depth)| **depth == 0)
        .map(|(node, _)| node.clone())
        .collect();
    let totals = if is_acyclic(&adjacency, seen) {
        layered(&adjacency, &starts, steps, query, property)?
    } else {
        enumerated(&adjacency, &starts, steps, query, property)?
    };
    for (node, (sum, count)) in totals {
        seen.rolled.insert(node.clone(), sum);
        seen.counts.insert(node, count);
    }
    Ok(())
}

/// The edges between accepted nodes that pass the edge filter, with their factor. Edges out of a
/// node at the deepest level are useless (a path through them is longer than `max_depth`).
async fn adjacency_of<S: NodeSource>(
    source: &mut S,
    steps: &[Step],
    query: &GraphQuery,
    who: &Invocation<'_>,
    seen: &Seen,
    needs: &Needs,
    property: &spot_storage_contract::Name,
) -> Result<Adjacency, GraphQueryError> {
    let mut adjacency = Adjacency::new();
    for (node, depth) in &seen.depth {
        let mut edges = Vec::new();
        if *depth < query.max_depth
            && let Some(node_data) = seen.data.get(node)
        {
            for (index, step) in steps.iter().enumerate() {
                for reached in source.step(node, step, query.max_nodes).await? {
                    if reached == *node {
                        continue;
                    }
                    let Some(reached_data) = seen.data.get(&reached) else {
                        continue;
                    };
                    let props = edge_props_of(source, node, &reached, step, needs).await?;
                    if !edge_passes(
                        query,
                        who,
                        step,
                        (node, node_data),
                        (&reached, reached_data),
                        props.as_ref(),
                    ) {
                        continue;
                    }
                    let factor = props
                        .as_ref()
                        .and_then(|p| p.get(property))
                        .cloned()
                        .unwrap_or(Value::Null);
                    // Checked here so a non-numeric factor fails whichever path reaches it.
                    rollup_step(property, step, None, &factor)?;
                    edges.push(Edge {
                        to: reached,
                        step: index,
                        factor,
                    });
                }
            }
        }
        adjacency.insert(node.clone(), edges);
    }
    Ok(adjacency)
}

/// Whether the accepted subgraph has no cycle (Kahn's algorithm).
fn is_acyclic(adjacency: &Adjacency, seen: &Seen) -> bool {
    let mut indegree: BTreeMap<&NodeRef, usize> = seen.depth.keys().map(|n| (n, 0)).collect();
    for edges in adjacency.values() {
        for edge in edges {
            if let Some(count) = indegree.get_mut(&edge.to) {
                *count += 1;
            }
        }
    }
    let mut ready: Vec<&NodeRef> = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(node, _)| *node)
        .collect();
    let mut processed = 0usize;
    while let Some(node) = ready.pop() {
        processed += 1;
        for edge in adjacency.get(node).into_iter().flatten() {
            if let Some(count) = indegree.get_mut(&edge.to) {
                *count -= 1;
                if *count == 0 {
                    ready.push(&edge.to);
                }
            }
        }
    }
    processed == indegree.len()
}

fn overflow(step: &Step, property: &spot_storage_contract::Name) -> GraphQueryError {
    GraphQueryError::RollupOverflow {
        edge: step.name.clone(),
        property: property.as_str().to_owned(),
    }
}

/// Adds `(value, count)` into `into[node]`.
fn merge(
    into: &mut BTreeMap<NodeRef, Entry>,
    node: &NodeRef,
    (value, count): Entry,
    step: &Step,
    property: &spot_storage_contract::Name,
) -> Result<(), GraphQueryError> {
    match into.get_mut(node) {
        Some((sum, paths)) => {
            *sum = exact_sum(sum, &value).ok_or_else(|| overflow(step, property))?;
            *paths = paths.saturating_add(count);
        }
        None => {
            into.insert(node.clone(), (value, count));
        }
    }
    Ok(())
}

fn too_many(query: &GraphQuery) -> GraphQueryError {
    GraphQueryError::TooManyPaths {
        max_paths: query.max_paths,
    }
}

/// The acyclic case: layer `k` holds, per node, the sum and count over the paths of exactly `k`
/// edges; the answer is the sum over layers.
fn layered(
    adjacency: &Adjacency,
    starts: &[NodeRef],
    steps: &[Step],
    query: &GraphQuery,
    property: &spot_storage_contract::Name,
) -> Result<BTreeMap<NodeRef, Entry>, GraphQueryError> {
    let mut work: u64 = 0;
    let bound = u64::from(query.max_paths);
    let mut totals: BTreeMap<NodeRef, Entry> = BTreeMap::new();
    let mut layer: BTreeMap<NodeRef, Entry> = BTreeMap::new();
    for start in starts {
        for edge in adjacency.get(start).into_iter().flatten() {
            work += 1;
            if work > bound {
                return Err(too_many(query));
            }
            merge(
                &mut layer,
                &edge.to,
                (edge.factor.clone(), 1),
                &steps[edge.step],
                property,
            )?;
        }
    }
    for _length in 1..=query.max_depth {
        if layer.is_empty() {
            break;
        }
        let mut next: BTreeMap<NodeRef, Entry> = BTreeMap::new();
        for (node, (sum, count)) in &layer {
            for edge in adjacency.get(node).into_iter().flatten() {
                work += 1;
                if work > bound {
                    return Err(too_many(query));
                }
                let step = &steps[edge.step];
                let product = rollup_step(property, step, Some(sum), &edge.factor)?;
                merge(&mut next, &edge.to, (product, *count), step, property)?;
            }
        }
        for (node, entry) in &layer {
            // The step used for an overflow message here is any edge into the node; the sums
            // across layers share the node, and the first edge type is a good enough name.
            let step = steps.first().ok_or_else(|| too_many(query))?;
            merge(&mut totals, node, entry.clone(), step, property)?;
        }
        layer = next;
    }
    Ok(totals)
}

/// One level of the iterative depth-first enumeration.
struct Frame<'a> {
    node: &'a NodeRef,
    next_edge: usize,
    product: Option<Value>,
}

/// The cyclic case: every simple path from each start, one by one.
fn enumerated(
    adjacency: &Adjacency,
    starts: &[NodeRef],
    steps: &[Step],
    query: &GraphQuery,
    property: &spot_storage_contract::Name,
) -> Result<BTreeMap<NodeRef, Entry>, GraphQueryError> {
    let mut paths: u64 = 0;
    let bound = u64::from(query.max_paths);
    let mut totals: BTreeMap<NodeRef, Entry> = BTreeMap::new();
    let max_edges = usize::from(query.max_depth);
    for start in starts {
        let mut on_path: BTreeSet<&NodeRef> = BTreeSet::from([start]);
        let mut stack = vec![Frame {
            node: start,
            next_edge: 0,
            product: None,
        }];
        while !stack.is_empty() {
            let length = stack.len();
            let Some(top) = stack.last_mut() else {
                break;
            };
            let edges = adjacency.get(top.node).map_or(&[][..], Vec::as_slice);
            let Some(edge) = edges.get(top.next_edge) else {
                on_path.remove(top.node);
                stack.pop();
                continue;
            };
            top.next_edge += 1;
            if on_path.contains(&edge.to) || length > max_edges {
                continue;
            }
            paths += 1;
            if paths > bound {
                return Err(too_many(query));
            }
            let step = &steps[edge.step];
            let product = rollup_step(property, step, top.product.as_ref(), &edge.factor)?;
            merge(&mut totals, &edge.to, (product.clone(), 1), step, property)?;
            on_path.insert(&edge.to);
            stack.push(Frame {
                node: &edge.to,
                next_edge: 0,
                product: Some(product),
            });
        }
    }
    Ok(totals)
}
