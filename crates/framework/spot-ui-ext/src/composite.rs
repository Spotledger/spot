//! Composite helpers: the component reference graph, its cycles and the size of an expansion.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::Node;

/// The component ids a body uses, including those inside `fill` bodies.
pub(crate) fn dependencies(body: &Node, found: &mut BTreeSet<String>) {
    match body {
        Node::Row { children, .. }
        | Node::Column { children, .. }
        | Node::Grid { children, .. }
        | Node::Group { children, .. } => {
            for child in children {
                dependencies(child, found);
            }
        }
        Node::Repeat { body, .. } => dependencies(body, found),
        Node::When { then, .. } => dependencies(then, found),
        Node::Use {
            component, fill, ..
        } => {
            found.insert(component.clone());
            for body in fill.values() {
                dependencies(body, found);
            }
        }
        Node::Control { .. } | Node::Slot { .. } => {}
    }
}

/// The components that sit on a cycle of the reference graph.
pub(crate) fn cycles(graph: &BTreeMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    let mut state: BTreeMap<&str, u8> = BTreeMap::new();
    let mut cyclic: BTreeSet<String> = BTreeSet::new();
    for id in graph.keys() {
        if !state.contains_key(id.as_str()) {
            visit(id, graph, &mut state, &mut cyclic);
        }
    }
    cyclic
}

/// Depth-first search marking the members of every cycle (1 = on the stack, 2 = done).
fn visit<'a>(
    id: &'a str,
    graph: &'a BTreeMap<String, BTreeSet<String>>,
    state: &mut BTreeMap<&'a str, u8>,
    cyclic: &mut BTreeSet<String>,
) {
    state.insert(id, 1);
    if let Some(edges) = graph.get(id) {
        for edge in edges {
            match state.get(edge.as_str()).copied() {
                Some(1) => {
                    cyclic.insert(id.to_owned());
                    cyclic.insert(edge.clone());
                }
                Some(_) => {}
                None => visit(edge, graph, state, cyclic),
            }
        }
    }
    state.insert(id, 2);
}

/// The (depth, node count) of expanding `body`, or `None` when a `use` names an absent component
/// or the graph loops (the caller reports those separately).
pub(crate) fn measure(body: &Node, bodies: &BTreeMap<String, &Node>) -> Option<(u64, u64)> {
    let mut measurer = Measurer {
        bodies,
        memo: BTreeMap::new(),
        active: Vec::new(),
    };
    measurer.body(body)
}

struct Measurer<'a> {
    bodies: &'a BTreeMap<String, &'a Node>,
    memo: BTreeMap<String, (u64, u64)>,
    active: Vec<String>,
}

impl Measurer<'_> {
    fn body(&mut self, node: &Node) -> Option<(u64, u64)> {
        match node {
            Node::Row { children, .. }
            | Node::Column { children, .. }
            | Node::Grid { children, .. }
            | Node::Group { children, .. } => {
                let mut depth: u64 = 0;
                let mut nodes: u64 = 1;
                for child in children {
                    let (child_depth, child_nodes) = self.body(child)?;
                    depth = depth.max(child_depth);
                    nodes = nodes.saturating_add(child_nodes);
                }
                Some((depth.saturating_add(1), nodes))
            }
            Node::Repeat { max, body, .. } => {
                let (depth, nodes) = self.body(body)?;
                Some((
                    depth.saturating_add(1),
                    nodes.saturating_mul(u64::from(*max)).saturating_add(1),
                ))
            }
            Node::When { then, .. } => {
                let (depth, nodes) = self.body(then)?;
                Some((depth.saturating_add(1), nodes.saturating_add(1)))
            }
            Node::Use {
                component, fill, ..
            } => {
                if self.active.iter().any(|active| active == component) {
                    return None;
                }
                let used = *self.bodies.get(component)?;
                let (body_depth, body_nodes) = if let Some(cached) = self.memo.get(component) {
                    *cached
                } else {
                    self.active.push(component.clone());
                    let measured = self.body(used);
                    self.active.pop();
                    let measured = measured?;
                    self.memo.insert(component.clone(), measured);
                    measured
                };
                let mut depth = body_depth;
                let mut nodes = body_nodes;
                for filled in fill.values() {
                    let (fill_depth, fill_nodes) = self.body(filled)?;
                    depth = depth.max(fill_depth);
                    nodes = nodes.saturating_add(fill_nodes);
                }
                Some((depth.saturating_add(1), nodes.saturating_add(1)))
            }
            Node::Control { .. } | Node::Slot { .. } => Some((1, 1)),
        }
    }
}
