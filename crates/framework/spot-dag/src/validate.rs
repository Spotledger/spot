//! Validation (brief §2): every malformed graph is reported, not just the
//! first problem.
//!
//! [`validate`] walks a [`DagDefinition`] and returns every error it finds as
//! a `Vec<DagError>` sorted deterministically (by error code, then by node
//! id), so the same broken definition always yields the same list. All graph
//! traversal is iterative over an explicit stack: a 10,000-node chain must
//! not overflow the call stack (brief hard rule).
//!
//! The shape rules follow ADR-13: definitions live in memory, they are pure
//! data, and this module only judges their structure — never `kind`, `config`
//! contents or edge conditions.
//!
//! One documented scope rule: **shape errors are only produced for graphs
//! whose node universe is otherwise usable** — `NoEntry`, `NoTerminal`,
//! `Unreachable` and `DeadEnd` are checked strictly only when the definition
//! has no multi-member cycle. A cyclic graph cannot satisfy the shape rules
//! *because* of the cycle (every node on a ring has in- and out-degree and
//! usually no entry/terminal at all), so flooding the error list with one
//! Unreachable/DeadEnd row per node would repeat the same root problem
//! per-node; the `Cycle` row carries the diagnosis. Single-node self loops
//! still get their `SelfLoop` row plus shape rows (a self loop alone is not a
//! "real" cycle component). Identity, limit, float and version errors are
//! always reported, cyclic or not.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::DagError;
use crate::model::{DagDefinition, Node, NodeId};

/// Structural caps a definition must fit (defaults from the brief).
///
/// Every field can be raised or lowered by the caller; the defaults are
/// conservative because definitions are compiled into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most nodes one definition may contain (default 10,000).
    pub max_nodes: usize,
    /// The most edges one definition may contain (default 50,000).
    pub max_edges: usize,
    /// The largest serialised `config` of one node, in bytes (default 65,536).
    pub max_config_bytes: usize,
}

impl Limits {
    /// The brief's default limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_nodes: 10_000,
            max_edges: 50_000,
            max_config_bytes: 65_536,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

/// Validates a definition, returning **every** problem as a deterministically
/// sorted list (by error code, then by node id), or `Ok(())` when none.
///
/// Checks (brief §2): limits, identity (duplicate nodes/edges, unknown
/// endpoints, self loops, version 0, floats in config), acyclicity (one cycle
/// path per strongly connected component) and shape. Shape runs even when
/// cycles were found: a 2-cycle has no entry, so the caller sees both
/// `Cycle` and `NoEntry`. A zero-node definition is `DagError::Empty`.
///
/// The sort order is: code string ascending, then the anchoring node id
/// ascending; within one code + anchor, first-found order is kept. Anchors
/// are documented per variant in [`error_node`].
pub fn validate(def: &DagDefinition, limits: &Limits) -> Result<(), Vec<DagError>> {
    let mut errors = Vec::new();
    check_version(def, &mut errors);
    check_limits(def, limits, &mut errors);
    check_identity(def, &mut errors);
    check_config_floats(def, &mut errors);

    let index = index_nodes(&def.nodes);
    check_endpoints(def, &index, &mut errors);
    let self_loops = self_loop_nodes(def, &index);
    for id in &self_loops {
        errors.push(DagError::SelfLoop(id.clone()));
    }
    let cyclic_sccs = check_cycles(def, &index, &mut errors);
    // Shape rows for graphs with multi-member cycles would repeat the cycle's
    // own diagnosis once per node (every node on a ring has in- and out-
    // degree and often no entry/terminal at all); the `Cycle` row carries that
    // diagnosis. Self-loop graphs (no multi-member SCC) still get their shape
    // rows — see the module doc.
    if cyclic_sccs == 0 {
        check_shape(def, &index, &mut errors);
    }
    if def.nodes.is_empty() {
        errors.push(DagError::Empty);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        errors.sort_by(sort_key);
        Err(errors)
    }
}

/// The deterministic sort key: code first, then anchor node id. Idless errors
/// anchor `None` and sort ahead of anchored ones within the same code.
fn sort_key(a: &DagError, b: &DagError) -> std::cmp::Ordering {
    (a.code(), error_node(a)).cmp(&(b.code(), error_node(b)))
}

/// The node id that anchors an error in the sorted list, when its code carries
/// one:
///
/// - id-carrying variants anchor on their `NodeId`;
/// - `UnknownNode` anchors on the missing id (its `edge_index` stays
///   first-found within equal anchors);
/// - `Cycle` anchors on the cycle path's first node id;
/// - `DuplicateEdge` anchors on its `from` id;
/// - everything else anchors `None`.
fn error_node(error: &DagError) -> Option<&str> {
    match error {
        DagError::FloatInConfig { node, .. }
        | DagError::DuplicateNode(node)
        | DagError::SelfLoop(node)
        | DagError::Unreachable(node)
        | DagError::DeadEnd(node)
        | DagError::ConfigTooLarge { node, .. }
        | DagError::DuplicateEdge { from: node, .. }
        | DagError::UnknownNode { id: node, .. } => Some(node.as_str()),
        DagError::Cycle { path } => path.first().map(NodeId::as_str),
        DagError::NodeIdInvalid { .. }
        | DagError::KindInvalid { .. }
        | DagError::Empty
        | DagError::VersionZero
        | DagError::NoEntry
        | DagError::NoTerminal
        | DagError::TooManyNodes { .. }
        | DagError::TooManyEdges { .. }
        | DagError::Json { .. } => None,
    }
}

/// Version 0 is always an error, reported once per definition.
fn check_version(def: &DagDefinition, errors: &mut Vec<DagError>) {
    if def.version == 0 {
        errors.push(DagError::VersionZero);
    }
}

/// Limit checks: node count, edge count and per-node config size.
fn check_limits(def: &DagDefinition, limits: &Limits, errors: &mut Vec<DagError>) {
    if def.nodes.len() > limits.max_nodes {
        errors.push(DagError::TooManyNodes {
            limit: limits.max_nodes,
            actual: def.nodes.len(),
        });
    }
    if def.edges.len() > limits.max_edges {
        errors.push(DagError::TooManyEdges {
            limit: limits.max_edges,
            actual: def.edges.len(),
        });
    }
    for node in &def.nodes {
        let bytes = serde_json::to_vec(&node.config).map_or(usize::MAX, |v| v.len());
        if bytes > limits.max_config_bytes {
            errors.push(DagError::ConfigTooLarge {
                node: node.id.clone(),
                limit: limits.max_config_bytes,
                actual: bytes,
            });
        }
    }
}

/// Identity checks: duplicate nodes and duplicate edges.
fn check_identity(def: &DagDefinition, errors: &mut Vec<DagError>) {
    let mut seen_nodes: BTreeSet<&str> = BTreeSet::new();
    for node in &def.nodes {
        if !seen_nodes.insert(node.id.as_str()) {
            errors.push(DagError::DuplicateNode(node.id.clone()));
        }
    }
    let mut seen_edges: BTreeSet<(&str, &str, Option<&str>)> = BTreeSet::new();
    for edge in &def.edges {
        if !seen_edges.insert((
            edge.from.as_str(),
            edge.to.as_str(),
            edge.condition.as_deref(),
        )) {
            errors.push(DagError::DuplicateEdge {
                from: edge.from.clone(),
                to: edge.to.clone(),
                condition: edge.condition.clone(),
            });
        }
    }
}

/// Config float checks for every node (parsed JSON is already rejected, but
/// `validate` stays self-sufficient when values are built in code).
fn check_config_floats(def: &DagDefinition, errors: &mut Vec<DagError>) {
    for node in &def.nodes {
        if let Some(path) = crate::model::find_float(&node.config, String::new()) {
            errors.push(DagError::FloatInConfig {
                node: node.id.clone(),
                path: path.into_boxed_str(),
            });
        }
    }
}

/// Maps node id -> first position in `def.nodes`.
fn index_nodes(nodes: &[Node]) -> BTreeMap<&str, usize> {
    nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect()
}

/// Endpoint checks: every edge endpoint must name a known node.
fn check_endpoints(def: &DagDefinition, index: &BTreeMap<&str, usize>, errors: &mut Vec<DagError>) {
    for (i, edge) in def.edges.iter().enumerate() {
        for endpoint in [&edge.from, &edge.to] {
            if !index.contains_key(endpoint.as_str()) {
                errors.push(DagError::UnknownNode {
                    edge_index: i,
                    id: endpoint.clone(),
                });
            }
        }
    }
}

/// The ids of nodes with an edge to themselves, ascending.
///
/// Every id in `def.nodes` was grammar-checked by [`NodeId::deserialize`]
/// or [`NodeId::new`], so the ids found in edges that name existing nodes
/// revalidate successfully; the map from name back to `NodeId` cannot fail.
fn self_loop_nodes(def: &DagDefinition, index: &BTreeMap<&str, usize>) -> Vec<NodeId> {
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for edge in &def.edges {
        if edge.from.as_str() == edge.to.as_str() && index.contains_key(edge.from.as_str()) {
            ids.insert(edge.from.as_str());
        }
    }
    let mut out: Vec<NodeId> = Vec::with_capacity(ids.len());
    for id in ids {
        // `index` only contains ids that came from validated `Node`s, so this
        // rebuild never misses; the defensive fallback keeps total and is
        // unreachable in practice.
        match NodeId::new(id) {
            Ok(node_id) => out.push(node_id),
            Err(_) => continue,
        }
    }
    out
}

/// Cycle detection: every strongly connected component with more than one
/// member is reported as one concrete cycle path (brief §2: "one cycle per
/// strongly connected component"). Self loops are excluded here; they have
/// their own error row.
///
/// Iterative Tarjan over an explicit frame stack: the recursion-free form
/// runs the same steps as the textbook recursion, so low-links come out
/// identical. A 10,000-node chain costs heap, not stack.
///
/// Returns the number of cyclic (multi-member) SCCs it reported.
fn check_cycles(
    def: &DagDefinition,
    index: &BTreeMap<&str, usize>,
    errors: &mut Vec<DagError>,
) -> usize {
    let n = def.nodes.len();
    // Adjacency over real node indexes; self loops are excluded (they have
    // their own error and must not resurface inside a `Cycle` path).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for edge in &def.edges {
        if let (Some(&from), Some(&to)) =
            (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
            && from != to
            && !adj[from].contains(&to)
        {
            adj[from].push(to);
        }
    }

    // Iterative Tarjan SCC, one explicit frame per parent on the DFS path.
    let mut next_index: usize = 0;
    let mut stack_index: Vec<usize> = vec![usize::MAX; n];
    let mut low_link: Vec<usize> = vec![usize::MAX; n];
    let mut on_scc_stack: Vec<bool> = vec![false; n];
    let mut scc_stack: Vec<usize> = Vec::new();
    let mut scc_of: Vec<usize> = vec![usize::MAX; n];
    let mut scc_size: Vec<usize> = Vec::new();
    // Frames: (node, next adjacency cursor) — plain data, no recursion.
    let mut frames: Vec<(usize, usize)> = Vec::new();

    for root in 0..n {
        if stack_index[root] != usize::MAX {
            continue;
        }
        stack_index[root] = next_index;
        low_link[root] = next_index;
        next_index += 1;
        scc_stack.push(root);
        on_scc_stack[root] = true;
        frames.push((root, 0usize));

        while let Some(&mut (v, ref mut cursor)) = frames.last_mut() {
            if *cursor < adj[v].len() {
                let w = adj[v][*cursor];
                *cursor += 1;
                if stack_index[w] == usize::MAX {
                    stack_index[w] = next_index;
                    low_link[w] = next_index;
                    next_index += 1;
                    scc_stack.push(w);
                    on_scc_stack[w] = true;
                    frames.push((w, 0));
                } else if on_scc_stack[w] {
                    // Back edge to a node still on the SCC stack.
                    low_link[v] = low_link[v].min(stack_index[w]);
                }
            } else {
                // Finished `v`: pop the frame and fold the low-link up.
                frames.pop();
                if let Some(&mut (parent, _)) = frames.last_mut() {
                    low_link[parent] = low_link[parent].min(low_link[v]);
                }
                if low_link[v] == stack_index[v] {
                    // `v` roots an SCC: pop its members off the stack.
                    let mut size = 0usize;
                    while let Some(w) = scc_stack.pop() {
                        on_scc_stack[w] = false;
                        scc_of[w] = scc_size.len();
                        size += 1;
                        if w == v {
                            break;
                        }
                    }
                    scc_size.push(size);
                }
            }
        }
    }

    // One cycle per cyclic SCC (more than one member). The adjacency indexes
    // need node names for the reported path, so `one_cycle_in_scc` works on
    // names via a lookup closure built here.
    let mut cyclic = 0usize;
    for (scc, &size) in scc_size.iter().enumerate() {
        if size > 1 {
            cyclic += 1;
            let members: Vec<usize> = (0..n).filter(|&i| scc_of[i] == scc).collect();
            let names: Vec<String> = members
                .iter()
                .map(|&i| def.nodes[i].id.as_str().to_owned())
                .collect();
            if let Some(path) = one_cycle_in_scc(&members, &names, &adj) {
                errors.push(DagError::Cycle { path });
            }
        }
    }
    cyclic
}

/// Finds one concrete cycle inside one strongly connected component with more
/// than one member (one is guaranteed to exist).
///
/// Starts at the component's smallest node id and DFS stays inside the
/// component (iterative, explicit frames) until stepping to a node already on
/// the current path; the slice from that node onward, closed with it, is the
/// reported cycle. The first closed walk found that way is reported: it is a
/// real cycle of the SCC, not a canonical form.
///
/// `names` is indexed by **member position** (the same order as `members`),
/// not by graph index — build it from `members` in the caller.
fn one_cycle_in_scc(
    members: &[usize],
    names: &[String],
    adj: &[Vec<usize>],
) -> Option<Vec<NodeId>> {
    let member_set: BTreeSet<usize> = members.iter().copied().collect();
    let start = members.iter().copied().min()?;
    let mut path: Vec<usize> = vec![start];
    let mut on_path: BTreeSet<usize> = BTreeSet::from([start]);
    let mut frames: Vec<(usize, usize)> = vec![(start, 0)];

    while let Some(&mut (v, ref mut cursor_ref)) = frames.last_mut() {
        let cursor = *cursor_ref;
        if cursor < adj[v].len() {
            let w = adj[v][cursor];
            *cursor_ref = cursor + 1;
            if member_set.contains(&w) && on_path.contains(&w) {
                // Closed a cycle: report the walk from `w` around to `w`.
                let cut = path.iter().position(|&p| p == w).unwrap_or(0);
                let name_of = |graph_index: usize| -> Option<String> {
                    let slot = members.iter().position(|&m| m == graph_index)?;
                    names.get(slot).cloned()
                };
                let mut cycle: Vec<NodeId> = Vec::new();
                for &i in &path[cut..] {
                    let Some(name) = name_of(i) else {
                        continue;
                    };
                    // Ids from `def.nodes` are grammar-checked; a defensive
                    // skip keeps this total and never trips in practice.
                    match NodeId::new(&name) {
                        Ok(id) => cycle.push(id),
                        Err(_) => continue,
                    }
                }
                if let Some(first) = cycle.first() {
                    cycle.push(first.clone());
                }
                return Some(cycle);
            }
            if member_set.contains(&w) {
                path.push(w);
                on_path.insert(w);
                frames.push((w, 0));
            }
        } else {
            frames.pop();
            if let Some(popped) = path.pop() {
                on_path.remove(&popped);
            }
        }
    }
    None
}

/// Shape checks (brief §2.4): at least one entry (no incoming edge) and one
/// terminal (no outgoing edge), reachability from entries and to terminals.
///
/// Only runs when the definition has no multi-member cycle (the caller skips
/// it otherwise): on a cyclic graph every node sits on a ring, has in- and
/// out-degree and usually no entry or terminal, so per-node rows would repeat
/// the cycle's own diagnosis.
///
/// Degree rule: **self edges do not count as in/out degree** for the shape
/// analysis — otherwise a self loop would make its node its own entry AND
/// terminal and mask the shape problem. Two consequences (documented in the
/// tests):
/// - a node whose only edge is its self edge keeps an `Unreachable` +
///   `DeadEnd` shape only when no other edge touches it — with the self edge
///   skipped for degree the node is exactly an isolated node;
/// - a zero-degree node (no non-self edges at all) sits outside the flow and
///   earns both `Unreachable` and `DeadEnd`.
fn check_shape(def: &DagDefinition, index: &BTreeMap<&str, usize>, errors: &mut Vec<DagError>) {
    let n = def.nodes.len();
    let mut has_incoming: Vec<bool> = vec![false; n];
    let mut has_outgoing: Vec<bool> = vec![false; n];
    // Self edges do not count as in/out degree for the shape analysis
    // (otherwise a self loop would make its node its own entry AND terminal
    // and mask the shape problem). Unknown endpoints are skipped; they were
    // already reported (and a node only named by a dangling edge gets its
    // Unreachable/DeadEnd or entry row from its real edges alone).
    for edge in &def.edges {
        if edge.from.as_str() == edge.to.as_str() {
            continue;
        }
        if let (Some(&from), Some(&to)) =
            (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        {
            has_outgoing[from] = true;
            has_incoming[to] = true;
        }
    }

    let entries: Vec<usize> = (0..n).filter(|&i| !has_incoming[i]).collect();
    let terminals: Vec<usize> = (0..n).filter(|&i| !has_outgoing[i]).collect();

    if n > 0 {
        if entries.is_empty() {
            errors.push(DagError::NoEntry);
        }
        if terminals.is_empty() {
            errors.push(DagError::NoTerminal);
        }
    }

    // Reachability from entries through every edge (self loops included: they
    // never block reachability, they just cannot start it).
    let reachable = reachable_from(&entries, def, index);
    let mut reaches_terminal = vec![false; n];
    for &terminal in &terminals {
        reaches_terminal[terminal] = true;
        // Reverse reachability from this terminal over backwards edges.
        let mut seen: BTreeSet<usize> = BTreeSet::from([terminal]);
        let mut stack: Vec<usize> = vec![terminal];
        while let Some(current) = stack.pop() {
            for edge in &def.edges {
                if let (Some(&from), Some(&to)) =
                    (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
                    && to == current
                    && !seen.contains(&from)
                {
                    seen.insert(from);
                    stack.push(from);
                }
            }
        }
        for i in seen {
            reaches_terminal[i] = true;
        }
    }

    for node in &def.nodes {
        let i = index[node.id.as_str()];
        if !reachable[i] {
            errors.push(DagError::Unreachable(node.id.clone()));
        }
        if !reaches_terminal[i] {
            errors.push(DagError::DeadEnd(node.id.clone()));
        }
    }

    // The general rows above can miss a *pure island*: a node named by no edge
    // at all is an entry AND a terminal by the degree rule, so it seeds the
    // reachability walk (reachable) and reaches itself as a terminal. Beside a
    // real flow that is wrong — the island can neither start the flow nor be
    // reached by it. When the definition carries at least one plain (non-self)
    // edge, every completely edge-less node explicitly earns `Unreachable` and
    // `DeadEnd` (once each; the general rows above stay silent for it). A node
    // whose only edges are self loops is exempt: with self edges skipped for
    // degree it is its own entry and terminal, and its `SelfLoop` row is the
    // diagnosis. With no plain edge at all, every node is self-contained and
    // the general rows are the whole truth.
    if def.edges.iter().any(|e| e.from.as_str() != e.to.as_str()) {
        for node in &def.nodes {
            let touched = def
                .edges
                .iter()
                .any(|e| e.from.as_str() == node.id.as_str() || e.to.as_str() == node.id.as_str());
            if !touched {
                errors.push(DagError::Unreachable(node.id.clone()));
                errors.push(DagError::DeadEnd(node.id.clone()));
            }
        }
    }
}

/// Iterative forward reachability from a set of start nodes over every edge
/// (self loops included, they cannot leave their node anyway).
fn reachable_from(
    starts: &[usize],
    def: &DagDefinition,
    index: &BTreeMap<&str, usize>,
) -> Vec<bool> {
    let n = def.nodes.len();
    let mut seen: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = starts.to_vec();
    while let Some(current) = stack.pop() {
        if seen[current] {
            continue;
        }
        seen[current] = true;
        for edge in &def.edges {
            if edge.from.as_str() == def.nodes[current].id.as_str()
                && let Some(&to) = index.get(edge.to.as_str())
                && !seen[to]
            {
                stack.push(to);
            }
        }
    }
    seen
}
