//! A deliberately naive oracle: the same question answered by a straightforward in-memory
//! breadth-first search over the records' own values, with no graph query API in the path.
//!
//! The oracle is the reference the property test compares the real walk against. It shares the
//! fact data the storage executor reads ([`NodeData`], as the in-memory backend reports it) and
//! the permission engine's read decision, but it computes neighbours by scanning the facts and
//! does the permission check itself, node by node. A node the caller may not read is never
//! returned and never counted; the result says only that something was removed
//! ([`Truncation::Permission`]).
//!
//! The traversal steps come from the graph query API's compiler (the name-to-field mapping is
//! shared, and deliberately so); the walk, the depths, the chosen predecessors, the quantities
//! and the truncation reason are computed here.

use std::collections::BTreeMap;

use spot_graph_model::GraphModel;
use spot_graph_query::{Invocation, NodeData, Step};
use spot_permissions::{Action, DocFacts, FieldFacts, FieldLevel, mask};
use spot_storage_contract::{Direction as StorageDirection, Name, NodeRef, Value};

use crate::error::LineageError;
use crate::plan::Plan;
use crate::quantity::Quantity;
use crate::query::{LevelTotal, LineageEdge, LineageNode, LineageResult, Truncation};

/// Answers `plan` over `facts` for `who`, naively.
///
/// # Errors
///
/// [`LineageError::Graph`] when the plan's edge types do not compile against `model`, and the
/// quantity errors of a bill-of-materials trace.
pub fn run(
    plan: &Plan,
    model: &GraphModel,
    facts: &BTreeMap<NodeRef, NodeData>,
    who: &Invocation<'_>,
) -> Result<LineageResult, LineageError> {
    let steps = plan.steps(model)?;
    let max_nodes = plan.max_nodes();
    let mut nodes: BTreeMap<NodeRef, LineageNode> = BTreeMap::new();
    let mut permission = false;
    let mut node_limit = false;

    // Level 0.
    let mut frontier: Vec<NodeRef> = Vec::new();
    for start in dedup_sorted(plan.start()) {
        let Some(data) = facts.get(&start) else {
            continue;
        };
        if !readable(&start, data, who) {
            continue;
        }
        if nodes.len() >= max_nodes as usize {
            node_limit = true;
            break;
        }
        frontier.push(start.clone());
        nodes.insert(
            start.clone(),
            LineageNode {
                node: start.clone(),
                depth: 0,
                via: None,
                fields: project(&start, data, plan, who),
                quantity: plan.bom().map(|bom| bom.basis),
            },
        );
    }

    let mut level = 1;
    while level <= plan.levels() && !frontier.is_empty() && !node_limit {
        let mut candidates: BTreeMap<NodeRef, Vec<(NodeRef, usize)>> = BTreeMap::new();
        for predecessor in &frontier {
            for (index, step) in steps.iter().enumerate() {
                for reached in neighbours(facts, predecessor, step) {
                    candidates
                        .entry(reached)
                        .or_default()
                        .push((predecessor.clone(), index));
                }
            }
        }
        let mut next: Vec<NodeRef> = Vec::new();
        for (node, pairs) in candidates {
            if nodes.contains_key(&node) {
                continue;
            }
            let Some(data) = facts.get(&node) else {
                continue;
            };
            if !readable(&node, data, who) {
                permission = true;
                continue;
            }
            let Some((predecessor, index)) = pairs.first() else {
                continue;
            };
            if nodes.len() >= max_nodes as usize {
                node_limit = true;
                break;
            }
            let fields = project(&node, data, plan, who);
            let quantity = match plan.bom() {
                Some(bom) => {
                    let field = bom.qty_field.as_str().to_owned();
                    let Some(value) = fields.get(&bom.qty_field) else {
                        return Err(LineageError::MissingQuantity { field });
                    };
                    let parent = nodes
                        .get(predecessor)
                        .and_then(|node| node.quantity)
                        .ok_or(LineageError::MissingQuantity { field })?;
                    Some(parent.checked_mul(Quantity::from_value(value)?)?)
                }
                None => None,
            };
            next.push(node.clone());
            nodes.insert(
                node.clone(),
                LineageNode {
                    node,
                    depth: level,
                    via: Some((predecessor.clone(), steps[*index].name.clone())),
                    fields,
                    quantity,
                },
            );
        }
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
    if truncated.is_none() && !frontier.is_empty() && probe(&steps, facts, &nodes, &frontier, who) {
        truncated = Some(Truncation::Depth {
            max_depth: plan.levels(),
        });
    }

    assemble(nodes, truncated)
}

/// Whether a visible node lies beyond the last walked level (the depth probe).
fn probe(
    steps: &[Step],
    facts: &BTreeMap<NodeRef, NodeData>,
    nodes: &BTreeMap<NodeRef, LineageNode>,
    frontier: &[NodeRef],
    who: &Invocation<'_>,
) -> bool {
    for predecessor in frontier {
        for step in steps {
            for reached in neighbours(facts, predecessor, step) {
                if nodes.contains_key(&reached) {
                    continue;
                }
                if let Some(data) = facts.get(&reached)
                    && readable(&reached, data, who)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// The nodes one step away from `node` along `step`, by scanning the facts.
fn neighbours(facts: &BTreeMap<NodeRef, NodeData>, node: &NodeRef, step: &Step) -> Vec<NodeRef> {
    let mut out: Vec<NodeRef> = Vec::new();
    let outgoing = matches!(
        step.direction,
        StorageDirection::Outgoing | StorageDirection::Both
    );
    let incoming = matches!(
        step.direction,
        StorageDirection::Incoming | StorageDirection::Both
    );
    if outgoing
        && node.entity == step.spec.entity
        && let Some(Value::Id(target)) = facts
            .get(node)
            .and_then(|data| data.values.get(&step.spec.field))
        && let Some(found) = facts.keys().find(|candidate| candidate.id == *target)
    {
        out.push(found.clone());
    }
    if incoming {
        for (holder, data) in facts {
            if holder.entity == step.spec.entity
                && data.values.get(&step.spec.field) == Some(&Value::Id(node.id))
            {
                out.push(holder.clone());
            }
        }
    }
    out.retain(|candidate| candidate != node);
    out.sort();
    out.dedup();
    out
}

/// The read decision, exactly as the graph query API applies it.
fn readable(node: &NodeRef, data: &NodeData, who: &Invocation<'_>) -> bool {
    who.engine
        .can(who.principal, Action::Read, &facts_of(node, data))
        .allowed
}

fn facts_of(node: &NodeRef, data: &NodeData) -> DocFacts {
    DocFacts {
        doctype: node.entity.as_str().to_owned(),
        id: Some(node.id.to_string()),
        created_by: data.created_by.clone(),
        state: data.state.clone(),
        values: data
            .values
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect(),
    }
}

/// The requested fields the caller may read, with the same masking the graph query API applies.
fn project(
    node: &NodeRef,
    data: &NodeData,
    plan: &Plan,
    who: &Invocation<'_>,
) -> BTreeMap<Name, Value> {
    let facts = facts_of(node, data);
    let doctype = node.entity.as_str();
    let snapshot = who.engine.snapshot();
    plan.fields()
        .iter()
        .filter_map(|field| {
            let field_facts = snapshot
                .facts(doctype)
                .as_deref()
                .and_then(|facts| facts.field(field.as_str()))
                .cloned()
                .unwrap_or_else(|| FieldFacts::plain(field.as_str()));
            let access = who
                .engine
                .field_access(who.principal, doctype, &field_facts, &facts);
            let stored = data.values.get(field).cloned().unwrap_or(Value::Null);
            match access.level {
                FieldLevel::Hidden => None,
                FieldLevel::Masked(format) => Some((
                    field.clone(),
                    match stored {
                        Value::Text(text) => Value::Text(mask(&text, &format)),
                        _ => Value::Null,
                    },
                )),
                FieldLevel::Read | FieldLevel::ReadReveal => Some((field.clone(), stored)),
            }
        })
        .collect()
}

fn dedup_sorted(mut nodes: Vec<NodeRef>) -> Vec<NodeRef> {
    nodes.sort();
    nodes.dedup();
    nodes
}

/// Builds the ordered result, independently of the real walk's assembly.
fn assemble(
    nodes: BTreeMap<NodeRef, LineageNode>,
    truncated: Option<Truncation>,
) -> Result<LineageResult, LineageError> {
    let mut ordered: Vec<LineageNode> = nodes.into_values().collect();
    ordered.sort_by(|a, b| (a.depth, &a.node).cmp(&(b.depth, &b.node)));
    let mut edges: Vec<LineageEdge> = ordered
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
    let mut totals: BTreeMap<u16, LevelTotal> = BTreeMap::new();
    for node in &ordered {
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
    Ok(LineageResult {
        nodes: ordered,
        edges,
        levels: totals.into_values().collect(),
        truncated,
    })
}
