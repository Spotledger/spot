//! Validation (brief §3): every problem is reported at once, never just the
//! first.
//!
//! [`validate`] walks a [`Process`] and returns every problem as a
//! deterministically sorted list of [`Problem`]s (by code, then version, then
//! node id). A process version is validated against the DocType metadata the
//! caller supplies through [`MetaView`], so it stays free of the metadata
//! crate. Expression conditions and rules are parsed with `spot-expr`.
//!
//! Ids, duplicate nodes/edges/rules, unknown endpoints, an unknown node kind,
//! cycles, a missing trigger, unreachable or dead-end nodes, ambiguous
//! branches, insufficient joins, unknown DocTypes or lifecycle operations,
//! expression errors, sub-process cycles and the size and depth limits are all
//! checked. The graph must be acyclic because it is compiled onto the DAG
//! runtime (`spot-dag`), which is acyclic by definition.

use std::collections::{BTreeMap, BTreeSet};

use crate::id::{NodeId, is_name};
use crate::model::{
    DocumentAction, DocumentConfig, JoinRule, KNOWN_OPERATIONS, Limits, LogicType,
    MAX_LOOP_ITERATIONS, MetaView, NodeKind, Process, ProcessVersion, TriggerConfig, TriggerType,
};

/// The stable `bpm_model.*` codes this crate reports.
pub mod codes {
    /// A version has no nodes.
    pub const EMPTY: &str = "bpm_model.empty";
    /// A version's number is 0; versions start at 1.
    pub const VERSION_ZERO: &str = "bpm_model.version_zero";
    /// A process or app name breaks the id grammar.
    pub const INVALID_NAME: &str = "bpm_model.invalid_name";
    /// A logic node sets a key its logic type does not read.
    pub const LOGIC_CONFIG_UNEXPECTED: &str = "bpm_model.logic_config_unexpected";
    /// A document node's service principal is empty, too long or has a control
    /// character or whitespace.
    pub const PRINCIPAL_INVALID: &str = "bpm_model.principal_invalid";
    /// A logic node's loop or map setting is out of range or empty.
    pub const LOGIC_CONFIG_INVALID: &str = "bpm_model.logic_config_invalid";
    /// A node's kind token breaks the kind grammar.
    pub const INVALID_KIND: &str = "bpm_model.invalid_kind";
    /// A float is inside a node's config (ADR-12).
    pub const FLOAT_IN_CONFIG: &str = "bpm_model.float_in_config";
    /// The same node id appears twice.
    pub const DUPLICATE_NODE: &str = "bpm_model.duplicate_node";
    /// The same edge appears twice.
    pub const DUPLICATE_EDGE: &str = "bpm_model.duplicate_edge";
    /// The same rule id appears twice.
    pub const DUPLICATE_RULE: &str = "bpm_model.duplicate_rule";
    /// An edge, error branch or sub-process names a node that is not declared.
    pub const UNKNOWN_NODE: &str = "bpm_model.unknown_node";
    /// A node kind this build does not know (ADR-080).
    pub const UNKNOWN_NODE_KIND: &str = "bpm_model.unknown_node_kind";
    /// An edge names a rule that is not declared.
    pub const UNKNOWN_RULE: &str = "bpm_model.unknown_rule";
    /// The graph has a cycle; it must be acyclic to compile onto the DAG.
    pub const CYCLE: &str = "bpm_model.cycle";
    /// The version has no trigger node.
    pub const MISSING_TRIGGER: &str = "bpm_model.missing_trigger";
    /// A node cannot be reached from any entry node.
    pub const UNREACHABLE: &str = "bpm_model.unreachable";
    /// A node with no outgoing edge is not marked terminal.
    pub const DEAD_END: &str = "bpm_model.dead_end";
    /// A node branches without distinct conditions or a default edge.
    pub const AMBIGUOUS_BRANCH: &str = "bpm_model.ambiguous_branch";
    /// A join needs more incoming edges than the node has.
    pub const JOIN_INSUFFICIENT: &str = "bpm_model.join_insufficient";
    /// A document node names a DocType that is not installed.
    pub const UNKNOWN_DOCTYPE: &str = "bpm_model.unknown_doctype";
    /// A document or trigger node names a lifecycle operation that does not
    /// exist.
    pub const UNKNOWN_OPERATION: &str = "bpm_model.unknown_operation";
    /// A trigger node is missing what its trigger type needs.
    pub const TRIGGER_INCOMPLETE: &str = "bpm_model.trigger_incomplete";
    /// An edge condition or a rule does not parse.
    pub const EXPRESSION: &str = "bpm_model.expression";
    /// A sub-process names a process that does not exist.
    pub const SUBPROCESS_UNKNOWN: &str = "bpm_model.subprocess_unknown";
    /// Sub-process references form a cycle.
    pub const SUBPROCESS_CYCLE: &str = "bpm_model.subprocess_cycle";
    /// The version has more nodes than the limit allows.
    pub const TOO_MANY_NODES: &str = "bpm_model.too_many_nodes";
    /// The version has more edges than the limit allows.
    pub const TOO_MANY_EDGES: &str = "bpm_model.too_many_edges";
    /// The longest chain is deeper than the limit allows.
    pub const TOO_DEEP: &str = "bpm_model.too_deep";
    /// A version's name or app does not match its process.
    pub const PROCESS_MISMATCH: &str = "bpm_model.process_mismatch";
    /// The compiled DAG rejected the graph (a backstop; validation runs first).
    pub const DAG_INVALID: &str = "bpm_model.dag_invalid";
}

/// A line and column (1-based) in an expression, numbers only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// The line, starting at 1.
    pub line: usize,
    /// The column, starting at 1.
    pub column: usize,
}

impl Position {}

/// Maps a 0-based character offset in `text` to a 1-based line and column.
#[must_use]
pub fn position_of(text: &str, offset: usize) -> Position {
    let mut line = 1usize;
    let mut column = 1usize;
    for ch in text.chars().take(offset) {
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Position { line, column }
}

/// One problem with a process, at one point in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The stable machine-readable code (`bpm_model.*`).
    pub code: &'static str,
    /// The version the problem was found in.
    pub version: u32,
    /// The node the problem anchors on, when there is one.
    pub node: Option<NodeId>,
    /// A position in an expression, when there is one (numbers only).
    pub position: Option<Position>,
    /// A human-readable sentence built from [`Self::code`] and the node id
    /// only (see [`Self::describe`]); never document or parser text.
    pub message: String,
}

impl Problem {
    /// Builds a problem anchored on a node.
    fn at(code: &'static str, version: u32, node: &NodeId) -> Self {
        Self::new(code, version, Some(node.clone()), None)
    }

    /// Builds a problem anchored on a node with an expression position.
    fn at_position(code: &'static str, version: u32, node: &NodeId, position: Position) -> Self {
        Self::new(code, version, Some(node.clone()), Some(position))
    }

    /// Builds a problem about a whole version.
    fn version(code: &'static str, version: u32) -> Self {
        Self::new(code, version, None, None)
    }

    /// Builds a problem and its text from the kind code and the node id.
    pub(crate) fn new(
        code: &'static str,
        version: u32,
        node: Option<NodeId>,
        position: Option<Position>,
    ) -> Self {
        let mut problem = Self {
            code,
            version,
            node,
            position,
            message: String::new(),
        };
        problem.message = problem.describe();
        problem
    }

    /// The human-readable text: a fixed sentence per kind code, plus the node
    /// id (grammar-checked `[a-z][a-z0-9_]*`, at most 64 characters) and the
    /// expression position when there are some. Nothing from the document's
    /// free text or from a parser message is ever part of it.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut text = describe_code(self.code).to_owned();
        if let Some(node) = &self.node {
            text.push_str(" (node ");
            text.push_str(node.as_str());
            text.push(')');
        }
        if let Some(position) = self.position {
            text.push_str(&format!(
                " at line {}, column {}",
                position.line, position.column
            ));
        }
        text
    }
}

/// The fixed sentence for a kind code; an unrecognised code gets a generic one.
#[must_use]
pub fn describe_code(code: &str) -> &'static str {
    match code {
        codes::EMPTY => "the version has no nodes",
        codes::VERSION_ZERO => "the version number is 0; versions start at 1",
        codes::INVALID_NAME => "a process or app name is not a valid name",
        codes::LOGIC_CONFIG_UNEXPECTED => "a logic setting does not belong to this logic type",
        codes::LOGIC_CONFIG_INVALID => "a logic setting has an invalid value",
        codes::PRINCIPAL_INVALID => "a document node's service principal is not valid",
        codes::INVALID_KIND => "a node's kind is not valid",
        codes::FLOAT_IN_CONFIG => "a node's settings contain a floating-point number",
        codes::DUPLICATE_NODE => "two nodes share one id",
        codes::DUPLICATE_EDGE => "two edges join the same pair of nodes",
        codes::DUPLICATE_RULE => "two rules share one id",
        codes::UNKNOWN_NODE => "an edge refers to a node that does not exist",
        codes::UNKNOWN_NODE_KIND => "a node has a kind this version does not know",
        codes::UNKNOWN_RULE => "an edge refers to a rule that does not exist",
        codes::CYCLE => "the graph contains a cycle",
        codes::MISSING_TRIGGER => "the process has no trigger",
        codes::UNREACHABLE => "a node cannot be reached from a trigger",
        codes::DEAD_END => "a node has no way out and is not a terminal",
        codes::AMBIGUOUS_BRANCH => "a branch has ambiguous outgoing edges",
        codes::JOIN_INSUFFICIENT => "a join has too few incoming edges",
        codes::UNKNOWN_DOCTYPE => "a node names a document type that does not exist",
        codes::UNKNOWN_OPERATION => "a node names an operation that is not known",
        codes::TRIGGER_INCOMPLETE => "a trigger is missing a required setting",
        codes::EXPRESSION => "an expression does not parse",
        codes::SUBPROCESS_UNKNOWN => "a sub-process names a process that does not exist",
        codes::SUBPROCESS_CYCLE => "sub-processes call each other in a cycle",
        codes::TOO_MANY_NODES => "the version has more nodes than the limit",
        codes::TOO_MANY_EDGES => "the version has more edges than the limit",
        codes::TOO_DEEP => "the graph is deeper than the limit",
        codes::PROCESS_MISMATCH => "a version belongs to a different process or app",
        codes::DAG_INVALID => "the graph layer rejected the process graph",
        _ => "the process has a problem",
    }
}

/// Lets `?` turn a single [`Problem`] into the `Vec<Problem>` a compile
/// returns.
impl From<Problem> for Vec<Problem> {
    fn from(problem: Problem) -> Self {
        vec![problem]
    }
}

/// Sorts problems deterministically: code, then version, then node id.
fn sort_problems(problems: &mut [Problem]) {
    problems.sort_by(|a, b| {
        (
            a.code,
            a.version,
            a.node.as_ref().map(NodeId::as_str),
            a.position.map(|p| p.line),
            a.position.map(|p| p.column),
        )
            .cmp(&(
                b.code,
                b.version,
                b.node.as_ref().map(NodeId::as_str),
                b.position.map(|p| p.line),
                b.position.map(|p| p.column),
            ))
    });
}

/// Validates a whole process: every version, plus that each version's name and
/// app match the process.
#[must_use]
pub fn validate(process: &Process, meta: &dyn MetaView, limits: &Limits) -> Vec<Problem> {
    let mut problems = Vec::new();
    if !is_name(&process.name) {
        problems.push(Problem::version(codes::INVALID_NAME, 0));
    }
    if !is_name(&process.app) {
        problems.push(Problem::version(codes::INVALID_NAME, 0));
    }
    let mut reported_mismatch: BTreeSet<u32> = BTreeSet::new();
    for version in &process.versions {
        let mismatched = version.name != process.name || version.app != process.app;
        if mismatched && reported_mismatch.insert(version.version) {
            problems.push(Problem::version(codes::PROCESS_MISMATCH, version.version));
        }
        problems.extend(validate_version(version, meta, limits));
    }
    sort_problems(&mut problems);
    problems
}

/// Validates one version.
#[must_use]
pub fn validate_version(
    version: &ProcessVersion,
    meta: &dyn MetaView,
    limits: &Limits,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    let v = version.version;

    check_version(version, &mut problems);
    check_names(version, &mut problems);
    check_limits(version, limits, &mut problems);
    check_duplicates(version, &mut problems);

    let index = index_nodes(version);
    check_endpoints(version, &index, &mut problems);
    check_error_branches(version, &index, &mut problems);
    check_kinds_and_floats(version, &mut problems);

    let graph = Graph::build(version, &index);
    let cycles = graph.cycles(version);
    for cycle in &cycles {
        problems.push(Problem::at(codes::CYCLE, v, &cycle[0]));
    }
    if cycles.is_empty() {
        check_shape(version, &graph, &mut problems);
        check_depth(version, &graph, limits, &mut problems);
    }

    check_missing_trigger(version, &mut problems);
    check_triggers(version, meta, &mut problems);
    check_documents(version, meta, &mut problems);
    check_forms(version, meta, &mut problems);
    check_semantics(version, &mut problems);
    check_logic_configs(version, &mut problems);
    check_branches(version, &mut problems);
    check_joins(version, &index, &mut problems);
    check_sub_processes(version, meta, &mut problems);

    problems.sort_by(|a, b| {
        (
            a.code,
            a.node.as_ref().map(NodeId::as_str),
            a.position.map(|p| p.line),
            a.position.map(|p| p.column),
        )
            .cmp(&(
                b.code,
                b.node.as_ref().map(NodeId::as_str),
                b.position.map(|p| p.line),
                b.position.map(|p| p.column),
            ))
    });
    problems
}

/// Version 0 is an error, reported once.
fn check_version(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    if version.version == 0 {
        problems.push(Problem::version(codes::VERSION_ZERO, version.version));
    }
}

/// The process and app names follow the id grammar.
fn check_names(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    if !is_name(&version.name) {
        problems.push(Problem::version(codes::INVALID_NAME, version.version));
    }
    if !is_name(&version.app) {
        problems.push(Problem::version(codes::INVALID_NAME, version.version));
    }
}

/// Node and edge counts and the graph shape fit the limits.
fn check_limits(version: &ProcessVersion, limits: &Limits, problems: &mut Vec<Problem>) {
    if version.nodes.is_empty() {
        problems.push(Problem::version(codes::EMPTY, version.version));
    }
    if version.nodes.len() > limits.max_nodes {
        problems.push(Problem::version(codes::TOO_MANY_NODES, version.version));
    }
    if version.edges.len() > limits.max_edges {
        problems.push(Problem::version(codes::TOO_MANY_EDGES, version.version));
    }
}

/// Duplicate nodes, edges and rules.
fn check_duplicates(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    let mut seen_nodes: BTreeSet<&str> = BTreeSet::new();
    for node in &version.nodes {
        if !seen_nodes.insert(node.id.as_str()) {
            problems.push(Problem::at(
                codes::DUPLICATE_NODE,
                version.version,
                &node.id,
            ));
        }
    }
    let mut seen_rules: BTreeSet<&str> = BTreeSet::new();
    for rule in &version.rules {
        if !seen_rules.insert(rule.id.as_str()) {
            problems.push(Problem::at(
                codes::DUPLICATE_RULE,
                version.version,
                &rule.id,
            ));
        }
    }
    let mut seen_edges: BTreeSet<(&str, &str, Option<&str>, Option<&str>)> = BTreeSet::new();
    for edge in &version.edges {
        if !seen_edges.insert((
            edge.from.as_str(),
            edge.to.as_str(),
            edge.condition.as_deref(),
            edge.rule.as_deref(),
        )) {
            problems.push(Problem::at(
                codes::DUPLICATE_EDGE,
                version.version,
                &edge.from,
            ));
        }
    }
}

/// Node id -> definition index.
fn index_nodes(version: &ProcessVersion) -> BTreeMap<String, usize> {
    version
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str().to_owned(), i))
        .collect()
}

/// Every edge endpoint names a declared node.
fn check_endpoints(
    version: &ProcessVersion,
    index: &BTreeMap<String, usize>,
    problems: &mut Vec<Problem>,
) {
    for edge in &version.edges {
        for endpoint in [&edge.from, &edge.to] {
            if !index.contains_key(endpoint.as_str()) {
                problems.push(Problem::at(codes::UNKNOWN_NODE, version.version, endpoint));
            }
        }
    }
}

/// Every error branch names a declared node.
fn check_error_branches(
    version: &ProcessVersion,
    index: &BTreeMap<String, usize>,
    problems: &mut Vec<Problem>,
) {
    for node in &version.nodes {
        if let Some(target) = &node.error
            && !index.contains_key(target.as_str())
        {
            problems.push(Problem::at(codes::UNKNOWN_NODE, version.version, &node.id));
        }
    }
}

/// Unknown kinds and floats in config.
fn check_kinds_and_floats(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    for node in &version.nodes {
        if !is_kind_token(node.kind.name()) {
            problems.push(Problem::at(codes::INVALID_KIND, version.version, &node.id));
        }
        if let NodeKind::Unknown(_) = &node.kind {
            problems.push(Problem::at(
                codes::UNKNOWN_NODE_KIND,
                version.version,
                &node.id,
            ));
        }
        let table = node.kind.config_table();
        if crate::tomlutil::find_float(&toml::Value::Table(table), "").is_some() {
            problems.push(Problem::at(
                codes::FLOAT_IN_CONFIG,
                version.version,
                &node.id,
            ));
        }
    }
}

/// Whether a kind token matches `[a-z][a-z0-9_.]{0,63}`.
fn is_kind_token(raw: &str) -> bool {
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || raw.len() > crate::id::MAX_NAME_LEN {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

/// The graph adjacency used by the shape, cycle and depth checks.
struct Graph {
    /// Adjacency by definition index.
    out: Vec<Vec<usize>>,
    /// Reverse adjacency by definition index.
    incoming: Vec<Vec<usize>>,
}

impl Graph {
    /// Builds adjacency, skipping unknown endpoints. An error branch is a real
    /// outgoing edge of its node (the DAG runtime has no separate error
    /// concept), so it is included here exactly as [`compile`](crate::compile)
    /// adds it to the definition.
    fn build(version: &ProcessVersion, index: &BTreeMap<String, usize>) -> Self {
        let n = version.nodes.len();
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut incoming: Vec<Vec<usize>> = vec![Vec::new(); n];
        for edge in &version.edges {
            if let (Some(&from), Some(&to)) =
                (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
            {
                out[from].push(to);
                incoming[to].push(from);
            }
        }
        for (from, node) in version.nodes.iter().enumerate() {
            if let Some(target) = &node.error
                && let Some(&to) = index.get(target.as_str())
            {
                out[from].push(to);
                incoming[to].push(from);
            }
        }
        for list in out.iter_mut() {
            list.sort_unstable();
            list.dedup();
        }
        for list in incoming.iter_mut() {
            list.sort_unstable();
            list.dedup();
        }
        Self { out, incoming }
    }

    /// Entry nodes (no incoming edge), ascending by definition index.
    fn entries(&self) -> Vec<usize> {
        (0..self.out.len())
            .filter(|&i| self.incoming[i].is_empty())
            .collect()
    }

    /// One concrete cycle per cyclic walk, in id order.
    fn cycles(&self, version: &ProcessVersion) -> Vec<Vec<NodeId>> {
        let n = self.out.len();
        // Kahn peel: nodes removed in topological order are acyclic.
        let mut indegree: Vec<usize> = self.incoming.iter().map(Vec::len).collect();
        let mut removed = vec![false; n];
        let mut queue: BTreeSet<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
        while let Some(&i) = queue.iter().next() {
            queue.remove(&i);
            removed[i] = true;
            for &to in &self.out[i] {
                indegree[to] -= 1;
                if indegree[to] == 0 {
                    queue.insert(to);
                }
            }
        }
        let remaining: Vec<bool> = (0..n).map(|i| !removed[i]).collect();
        let mut visited = vec![false; n];
        let mut cycles = Vec::new();
        for start in 0..n {
            if !remaining[start] || visited[start] {
                continue;
            }
            if let Some(cycle) = self.walk_cycle(start, &remaining, &mut visited, version) {
                cycles.push(cycle);
            }
        }
        cycles
    }

    /// Walks within the remaining (cyclic) subgraph until a node repeats.
    fn walk_cycle(
        &self,
        start: usize,
        remaining: &[bool],
        visited: &mut [bool],
        version: &ProcessVersion,
    ) -> Option<Vec<NodeId>> {
        let mut path: Vec<usize> = Vec::new();
        let mut on_path: BTreeSet<usize> = BTreeSet::new();
        let mut current = start;
        loop {
            if on_path.contains(&current) {
                let cut = path.iter().position(|&p| p == current)?;
                let mut ids: Vec<NodeId> = path[cut..]
                    .iter()
                    .map(|&i| version.nodes[i].id.clone())
                    .collect();
                if let Some(first) = ids.first() {
                    ids.push(first.clone());
                }
                return Some(ids);
            }
            if visited[current] {
                return None;
            }
            visited[current] = true;
            on_path.insert(current);
            path.push(current);
            let next = self.out[current].iter().copied().find(|&w| remaining[w])?;
            current = next;
        }
    }

    /// Reachable node indexes from the entries.
    fn reachable(&self) -> Vec<bool> {
        let mut seen = vec![false; self.out.len()];
        let mut stack = self.entries();
        while let Some(i) = stack.pop() {
            if seen[i] {
                continue;
            }
            seen[i] = true;
            for &to in &self.out[i] {
                if !seen[to] {
                    stack.push(to);
                }
            }
        }
        seen
    }
}

/// Shape: reachability from the entries, that a leaf is a terminal, and that a
/// node touched by no edge beside a real flow is an island.
///
/// The island rule mirrors the DAG runtime: a node named by no edge at all is
/// both an entry and a terminal to a degree analysis, so beside a real flow it
/// can neither start the flow nor be reached by it. When the version has no
/// edge at all, every node is self-contained and the general rows are the whole
/// truth.
fn check_shape(version: &ProcessVersion, graph: &Graph, problems: &mut Vec<Problem>) {
    let reachable = graph.reachable();
    let any_edge = graph.out.iter().any(|outs| !outs.is_empty());
    for (i, node) in version.nodes.iter().enumerate() {
        let island = any_edge && graph.out[i].is_empty() && graph.incoming[i].is_empty();
        if !reachable[i] || island {
            problems.push(Problem::at(codes::UNREACHABLE, version.version, &node.id));
        }
        if (graph.out[i].is_empty() && !node.terminal) || island {
            problems.push(Problem::at(codes::DEAD_END, version.version, &node.id));
        }
    }
}

/// The longest chain from an entry must fit `limits.max_depth`.
fn check_depth(
    version: &ProcessVersion,
    graph: &Graph,
    limits: &Limits,
    problems: &mut Vec<Problem>,
) {
    let n = graph.out.len();
    let mut indegree: Vec<usize> = graph.incoming.iter().map(Vec::len).collect();
    let mut queue: BTreeSet<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let mut depth = vec![0usize; n];
    let mut order = Vec::with_capacity(n);
    while let Some(&i) = queue.iter().next() {
        queue.remove(&i);
        order.push(i);
        for &to in &graph.out[i] {
            if depth[i] + 1 > depth[to] {
                depth[to] = depth[i] + 1;
            }
            indegree[to] -= 1;
            if indegree[to] == 0 {
                queue.insert(to);
            }
        }
    }
    if order.len() != n {
        return;
    }
    if let Some(&deepest) = depth.iter().max()
        && deepest > limits.max_depth
    {
        let i = depth.iter().position(|&d| d == deepest).unwrap_or(0);
        problems.push(Problem::at(
            codes::TOO_DEEP,
            version.version,
            &version.nodes[i].id,
        ));
    }
}

/// A version needs at least one trigger node.
fn check_missing_trigger(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    let has_trigger = version
        .nodes
        .iter()
        .any(|n| matches!(n.kind, NodeKind::Trigger(_)));
    if !has_trigger {
        problems.push(Problem::version(codes::MISSING_TRIGGER, version.version));
    }
}

/// Trigger node config is complete and its DocType exists.
fn check_triggers(version: &ProcessVersion, meta: &dyn MetaView, problems: &mut Vec<Problem>) {
    for node in &version.nodes {
        let NodeKind::Trigger(config) = &node.kind else {
            continue;
        };
        check_trigger(version, node, config, meta, problems);
    }
}

/// One trigger's completeness.
fn check_trigger(
    version: &ProcessVersion,
    node: &crate::model::Node,
    config: &TriggerConfig,
    meta: &dyn MetaView,
    problems: &mut Vec<Problem>,
) {
    match config.trigger {
        TriggerType::DocumentEvent => {
            let Some(doctype) = &config.doctype else {
                problems.push(Problem::at(
                    codes::TRIGGER_INCOMPLETE,
                    version.version,
                    &node.id,
                ));
                return;
            };
            if !meta.has_doctype(doctype) {
                problems.push(Problem::at(
                    codes::UNKNOWN_DOCTYPE,
                    version.version,
                    &node.id,
                ));
            }
            match &config.event {
                None => problems.push(Problem::at(
                    codes::TRIGGER_INCOMPLETE,
                    version.version,
                    &node.id,
                )),
                Some(event) if !KNOWN_OPERATIONS.contains(&event.as_str()) => {
                    problems.push(Problem::at(
                        codes::UNKNOWN_OPERATION,
                        version.version,
                        &node.id,
                    ));
                }
                Some(_) => {}
            }
        }
        TriggerType::Schedule => {
            if config.cron.is_none() {
                problems.push(Problem::at(
                    codes::TRIGGER_INCOMPLETE,
                    version.version,
                    &node.id,
                ));
            }
        }
        TriggerType::Webhook | TriggerType::Api => {
            if config.path.is_none() {
                problems.push(Problem::at(
                    codes::TRIGGER_INCOMPLETE,
                    version.version,
                    &node.id,
                ));
            }
        }
        TriggerType::Manual => {}
    }
}

/// Document nodes name an installed DocType and a real lifecycle operation.
fn check_documents(version: &ProcessVersion, meta: &dyn MetaView, problems: &mut Vec<Problem>) {
    for node in &version.nodes {
        let NodeKind::Document(config) = &node.kind else {
            continue;
        };
        check_document(version, node, config, meta, problems);
    }
}

/// The longest service principal name a document node may carry, in bytes.
const MAX_PRINCIPAL_BYTES: usize = 128;

/// One document node.
fn check_document(
    version: &ProcessVersion,
    node: &crate::model::Node,
    config: &DocumentConfig,
    meta: &dyn MetaView,
    problems: &mut Vec<Problem>,
) {
    if let Some(principal) = &config.principal
        && (principal.is_empty()
            || principal.len() > MAX_PRINCIPAL_BYTES
            || principal
                .chars()
                .any(|c| c.is_control() || c.is_whitespace()))
    {
        problems.push(Problem::at(
            codes::PRINCIPAL_INVALID,
            version.version,
            &node.id,
        ));
    }
    if !meta.has_doctype(&config.doctype) {
        problems.push(Problem::at(
            codes::UNKNOWN_DOCTYPE,
            version.version,
            &node.id,
        ));
        return;
    }
    if matches!(config.action, DocumentAction::Map) {
        return;
    }
    let operation = config.operation();
    if !meta.has_operation(&config.doctype, operation) {
        problems.push(Problem::at(
            codes::UNKNOWN_OPERATION,
            version.version,
            &node.id,
        ));
    }
}

/// A form node names an installed record type when it names one at all.
///
/// The rest of the form step's semantics — fields, assignee and draft mode —
/// belongs to `spot-bpm-contract`'s conformance check of a process half
/// (`bpm_contract.conformance_form`, WP-BPM-12), because a form step of a
/// conversation is checked with the contract it takes part in.
fn check_forms(version: &ProcessVersion, meta: &dyn MetaView, problems: &mut Vec<Problem>) {
    for node in &version.nodes {
        let NodeKind::Form(config) = &node.kind else {
            continue;
        };
        if let Some(record_type) = &config.record_type
            && !meta.has_doctype(record_type)
        {
            problems.push(Problem::at(
                codes::UNKNOWN_DOCTYPE,
                version.version,
                &node.id,
            ));
        }
    }
}

/// Rule expressions parse, and edges name a declared rule or parse a condition.
fn check_semantics(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    for rule in &version.rules {
        if let Err(error) = spot_expr::parse(&rule.expression) {
            let position = position_of(&rule.expression, error.offset);
            problems.push(Problem::at_position(
                codes::EXPRESSION,
                version.version,
                &rule.id,
                position,
            ));
        }
    }
    let rule_ids: BTreeSet<&str> = version.rules.iter().map(|r| r.id.as_str()).collect();
    for edge in &version.edges {
        if let Some(name) = &edge.rule
            && !rule_ids.contains(name.as_str())
        {
            problems.push(Problem::at(
                codes::UNKNOWN_RULE,
                version.version,
                &edge.from,
            ));
        }
        if let Some(condition) = &edge.condition
            && let Err(error) = spot_expr::parse(condition)
        {
            let position = position_of(condition, error.offset);
            problems.push(Problem::at_position(
                codes::EXPRESSION,
                version.version,
                &edge.from,
                position,
            ));
        }
    }
}

/// Loop and map settings are typed: each key is valid only on the logic types
/// that read it, counts are in range, and expressions are non-empty and parse
/// with the shared expression language (`spot-expr`).
fn check_logic_configs(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    for node in &version.nodes {
        let NodeKind::Logic(config) = &node.kind else {
            continue;
        };
        let logic = config.logic;
        let is_loop = logic == LogicType::Loop;
        let allowed = [
            ("max_iterations", config.max_iterations.is_some(), is_loop),
            ("condition", config.condition.is_some(), is_loop),
            ("body", !config.body.is_empty(), is_loop),
            (
                "mapping",
                !config.mapping.is_empty(),
                logic == LogicType::Map,
            ),
            (
                "branches",
                !config.branches.is_empty(),
                matches!(
                    logic,
                    LogicType::Branch | LogicType::Parallel | LogicType::Loop
                ),
            ),
        ];
        for (_, set, valid) in allowed {
            if set && !valid {
                problems.push(Problem::at(
                    codes::LOGIC_CONFIG_UNEXPECTED,
                    version.version,
                    &node.id,
                ));
            }
        }
        if let Some(max) = config.max_iterations
            && !(1..=MAX_LOOP_ITERATIONS).contains(&max)
        {
            problems.push(Problem::at(
                codes::LOGIC_CONFIG_INVALID,
                version.version,
                &node.id,
            ));
        }
        if let Some(condition) = &config.condition {
            check_logic_expression(version, &node.id, condition, problems);
        }
        for map in [&config.body, &config.mapping] {
            for (target, expression) in map {
                if target.trim().is_empty() {
                    problems.push(Problem::at(
                        codes::LOGIC_CONFIG_INVALID,
                        version.version,
                        &node.id,
                    ));
                }
                check_logic_expression(version, &node.id, expression, problems);
            }
        }
    }
}

/// A logic expression is not empty and parses.
fn check_logic_expression(
    version: &ProcessVersion,
    node: &NodeId,
    expression: &str,
    problems: &mut Vec<Problem>,
) {
    if expression.trim().is_empty() {
        problems.push(Problem::at(
            codes::LOGIC_CONFIG_INVALID,
            version.version,
            node,
        ));
    } else if let Err(error) = spot_expr::parse(expression) {
        let position = position_of(expression, error.offset);
        problems.push(Problem::at_position(
            codes::EXPRESSION,
            version.version,
            node,
            position,
        ));
    }
}

/// Branching needs distinct conditions or a default edge.
fn check_branches(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    let mut by_from: BTreeMap<&str, Vec<&crate::model::Edge>> = BTreeMap::new();
    for edge in &version.edges {
        by_from.entry(edge.from.as_str()).or_default().push(edge);
    }
    for node in &version.nodes {
        let Some(edges) = by_from.get(node.id.as_str()) else {
            continue;
        };
        if edges.len() < 2 || edges.iter().any(|e| e.default) {
            continue;
        }
        let mut seen: BTreeSet<(Option<&str>, Option<&str>)> = BTreeSet::new();
        let mut ambiguous = false;
        for edge in edges {
            let key = (edge.condition.as_deref(), edge.rule.as_deref());
            if key == (None, None) || !seen.insert(key) {
                ambiguous = true;
                break;
            }
        }
        if ambiguous {
            problems.push(Problem::at(
                codes::AMBIGUOUS_BRANCH,
                version.version,
                &node.id,
            ));
        }
    }
}

/// A quorum join needs at least as many incoming edges as it requires.
fn check_joins(
    version: &ProcessVersion,
    index: &BTreeMap<String, usize>,
    problems: &mut Vec<Problem>,
) {
    let mut incoming: Vec<usize> = vec![0; version.nodes.len()];
    for edge in &version.edges {
        if let Some(&to) = index.get(edge.to.as_str()) {
            incoming[to] += 1;
        }
    }
    for (i, node) in version.nodes.iter().enumerate() {
        if let JoinRule::Quorum { required } = node.join
            && (required == 0 || (incoming[i] as u32) < required)
        {
            problems.push(Problem::at(
                codes::JOIN_INSUFFICIENT,
                version.version,
                &node.id,
            ));
        }
    }
}

/// Sub-process references exist and do not cycle back to the process.
fn check_sub_processes(version: &ProcessVersion, meta: &dyn MetaView, problems: &mut Vec<Problem>) {
    for node in &version.nodes {
        let NodeKind::SubProcess(config) = &node.kind else {
            continue;
        };
        if !meta.has_process(&config.process) {
            problems.push(Problem::at(
                codes::SUBPROCESS_UNKNOWN,
                version.version,
                &node.id,
            ));
            continue;
        }
        if reaches_process(&config.process, &version.name, meta) {
            problems.push(Problem::at(
                codes::SUBPROCESS_CYCLE,
                version.version,
                &node.id,
            ));
        }
    }
}

/// Whether the sub-process graph starting at `start` reaches `target`.
fn reaches_process(start: &str, target: &str, meta: &dyn MetaView) -> bool {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![start.to_owned()];
    while let Some(current) = stack.pop() {
        if current == target {
            return true;
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        for next in meta.sub_processes(&current) {
            stack.push(next);
        }
    }
    false
}
