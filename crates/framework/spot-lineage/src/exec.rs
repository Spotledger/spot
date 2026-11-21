//! Running a compiled plan through a graph executor, level by level.
//!
//! The walk never sees a node the caller may not read: every level is a
//! [`spot_graph_query::GraphQuery`], so the graph query API's permission filter runs on every
//! reached record. A hidden node is removed with its whole path, is never counted in a level
//! total, and is reported only through [`Truncation::Permission`] — the redaction marker the
//! graph query API defines (`GraphResult::truncated_by_permission`).

use std::collections::BTreeMap;

use spot_graph_query::{GraphExecutor, GraphQueryError, Invocation, MAX_NODES_CEILING};
use spot_storage_contract::{ErrorKind, Name, NodeRef, Value};

use crate::error::LineageError;
use crate::plan::Plan;
use crate::quantity::Quantity;
use crate::query::{LevelTotal, LineageEdge, LineageNode, LineageResult, Truncation};

/// Runs `plan` through `executor` for `who`, one level at a time.
///
/// # Errors
///
/// [`LineageError::Graph`] for a graph query that fails for any reason other than exceeding the
/// node budget, and the quantity errors of a bill-of-materials trace.
pub async fn run<E: GraphExecutor>(
    executor: &mut E,
    plan: &Plan,
    who: &Invocation<'_>,
) -> Result<LineageResult, LineageError> {
    let max_nodes = plan.max_nodes();
    let mut nodes: BTreeMap<NodeRef, LineageNode> = BTreeMap::new();
    let mut permission = false;
    let mut node_limit = false;

    // The first query answers levels 0 and 1 at once: its depth-0 nodes are the readable start
    // records, its depth-1 nodes the first level.
    let first = match executor
        .execute(&plan.level_query(plan.start(), MAX_NODES_CEILING)?, who)
        .await
    {
        Ok(result) => result,
        Err(error) if over_budget_error(&error) => {
            return finish(nodes, Some(Truncation::Nodes { limit: max_nodes }));
        }
        Err(error) => return Err(error.into()),
    };
    permission |= first.truncated_by_permission;
    let mut frontier: Vec<NodeRef> = Vec::new();
    for node in &first.nodes {
        if node.depth != 0 || node_limit {
            continue;
        }
        if nodes.len() >= max_nodes as usize {
            node_limit = true;
            continue;
        }
        frontier.push(node.node.clone());
        nodes.insert(
            node.node.clone(),
            LineageNode {
                node: node.node.clone(),
                depth: 0,
                via: None,
                fields: node.fields.clone(),
                quantity: plan.bom().map(|bom| bom.basis),
            },
        );
    }
    frontier.sort();

    // Level 1, from the same query.
    let mut next: Vec<NodeRef> = Vec::new();
    for node in &first.nodes {
        if node.depth != 1 || node_limit {
            continue;
        }
        if nodes.contains_key(&node.node) {
            continue;
        }
        if nodes.len() >= max_nodes as usize {
            node_limit = true;
            continue;
        }
        let quantity = match &node.via {
            Some((predecessor, _)) => reached_quantity(plan, &nodes, predecessor, &node.fields)?,
            None => plan.bom().map(|bom| bom.basis),
        };
        next.push(node.node.clone());
        nodes.insert(
            node.node.clone(),
            LineageNode {
                node: node.node.clone(),
                depth: 1,
                via: node.via.clone(),
                fields: node.fields.clone(),
                quantity,
            },
        );
    }
    next.sort();
    frontier = next;

    // Levels 2 and up.
    let mut level = 2;
    while level <= plan.levels() && !frontier.is_empty() && !node_limit {
        let result = match executor
            .execute(&plan.level_query(frontier.clone(), MAX_NODES_CEILING)?, who)
            .await
        {
            Ok(result) => result,
            Err(error) if over_budget_error(&error) => {
                node_limit = true;
                break;
            }
            Err(error) => return Err(error.into()),
        };
        permission |= result.truncated_by_permission;
        let mut next: Vec<NodeRef> = Vec::new();
        for node in &result.nodes {
            if node.depth != 1 || nodes.contains_key(&node.node) || node_limit {
                continue;
            }
            if nodes.len() >= max_nodes as usize {
                node_limit = true;
                continue;
            }
            let quantity = match &node.via {
                Some((predecessor, _)) => {
                    reached_quantity(plan, &nodes, predecessor, &node.fields)?
                }
                None => plan.bom().map(|bom| bom.basis),
            };
            next.push(node.node.clone());
            nodes.insert(
                node.node.clone(),
                LineageNode {
                    node: node.node.clone(),
                    depth: level,
                    via: node.via.clone(),
                    fields: node.fields.clone(),
                    quantity,
                },
            );
        }
        next.sort();
        frontier = next;
        level += 1;
    }

    let mut truncated = if permission {
        Some(Truncation::Permission)
    } else if node_limit {
        Some(Truncation::Nodes { limit: max_nodes })
    } else {
        None
    };
    if truncated.is_none() && !frontier.is_empty() {
        let probe = plan.level_query(frontier, MAX_NODES_CEILING)?;
        let more = match executor.execute(&probe, who).await {
            Ok(result) => result
                .nodes
                .iter()
                .any(|node| node.depth == 1 && !nodes.contains_key(&node.node)),
            Err(error) if over_budget_error(&error) => true,
            Err(error) => return Err(error.into()),
        };
        if more {
            truncated = Some(Truncation::Depth {
                max_depth: plan.levels(),
            });
        }
    }

    finish(nodes, truncated)
}

/// Builds the ordered result from the accepted nodes.
///
/// # Errors
///
/// [`LineageError::QuantityOverflow`] when a level's summed quantity does not fit.
pub(crate) fn finish(
    nodes: BTreeMap<NodeRef, LineageNode>,
    truncated: Option<Truncation>,
) -> Result<LineageResult, LineageError> {
    let mut ordered: Vec<LineageNode> = nodes.into_values().collect();
    ordered.sort_by(|a, b| (a.depth, &a.node).cmp(&(b.depth, &b.node)));
    let edges = edges_of(&ordered);
    let levels = level_totals(&ordered)?;
    Ok(LineageResult {
        nodes: ordered,
        edges,
        levels,
        truncated,
    })
}

/// The edge list of an ordered node list: one edge per node reached at depth above 0.
pub(crate) fn edges_of(nodes: &[LineageNode]) -> Vec<LineageEdge> {
    let mut edges: Vec<LineageEdge> = nodes
        .iter()
        .filter_map(|node| {
            node.via.as_ref().map(|(from, edge)| LineageEdge {
                edge: edge.clone(),
                from: from.clone(),
                to: node.node.clone(),
            })
        })
        .collect();
    edges.sort_by(|a, b| (&a.from, &a.to, &a.edge).cmp(&(&b.from, &b.to, &b.edge)));
    edges
}

/// One total per level, ordered by depth.
pub(crate) fn level_totals(nodes: &[LineageNode]) -> Result<Vec<LevelTotal>, LineageError> {
    let mut totals: BTreeMap<u16, LevelTotal> = BTreeMap::new();
    for node in nodes {
        let total = totals.entry(node.depth).or_insert(LevelTotal {
            depth: node.depth,
            nodes: 0,
            quantity: None,
        });
        total.nodes += 1;
        if let Some(quantity) = node.quantity {
            total.quantity = Some(match total.quantity {
                Some(sum) => sum.checked_add(quantity)?,
                None => quantity,
            });
        }
    }
    Ok(totals.into_values().collect())
}

/// The quantity a reached node contributes: the parent's quantity times the per-unit quantity
/// read from the node's own readable fields.
pub(crate) fn reached_quantity(
    plan: &Plan,
    nodes: &BTreeMap<NodeRef, LineageNode>,
    predecessor: &NodeRef,
    fields: &BTreeMap<Name, Value>,
) -> Result<Option<Quantity>, LineageError> {
    let Some(bom) = plan.bom() else {
        return Ok(None);
    };
    let field = bom.qty_field.as_str().to_owned();
    let Some(value) = fields.get(&bom.qty_field) else {
        return Err(LineageError::MissingQuantity { field });
    };
    let multiplier = Quantity::from_value(value)?;
    let parent = nodes
        .get(predecessor)
        .and_then(|node| node.quantity)
        .ok_or(LineageError::MissingQuantity { field })?;
    Ok(Some(parent.checked_mul(multiplier)?))
}

fn over_budget_error(error: &GraphQueryError) -> bool {
    matches!(
        error,
        GraphQueryError::Storage(storage) if storage.kind == ErrorKind::TraversalTooLarge
    )
}
