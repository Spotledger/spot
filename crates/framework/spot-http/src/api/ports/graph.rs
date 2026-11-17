//! The graph and lineage seams of `POST /api/v1/graph/query` and `POST /api/v1/lineage`
//! (`graph_query` and `lineage_trace`, native API 0.14.0, WP-GRA-03.T6): a typed, permission-aware
//! walk over the business graph, and the named lineage questions.
//!
//! `spot-http` does not depend on the graph crates. It defines [`GraphPort`] and [`LineagePort`],
//! the two reads in the shapes the router needs, and the host writes the adapters over
//! `spot-graph-query` (WP-GRA-02.T2) and `spot-lineage` (WP-DAG-05.T2). Everything that decides
//! is the port's: who the caller is (from `ctx.credential` on `ctx.site`), whether the start
//! record is readable, and which nodes, edges and edge properties the caller may see. The routes
//! pass **no user, role or tenant**: there is no way to ask for another caller's view, or another
//! site's graph, through these traits.
//!
//! # Errors
//!
//! A port answers only with the codes the contract declares for the operation:
//! `graph.not_found` (a start record that does not exist and one the caller may not read are one
//! answer), `graph.query_invalid`, `graph.too_large`, `graph.unavailable` (the index is being
//! rebuilt) and, for lineage, `lineage.kind_unknown`. The routes turn any other error into
//! `500 http.internal` without detail.
//!
//! # Fakes
//!
//! [`MemoryGraph`] and [`MemoryLineage`] are the in-memory fakes the tests use. Each holds a
//! small fixed graph per site and a visibility set per token: a token that was never granted sees
//! nothing (fail closed), and a node outside a token's set is removed together with every path
//! through it, with only `withheld` saying so.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use rust_decimal::Decimal;
use spot_core::SiteId;

use super::PortKey;
use crate::api::registry::Code;
use crate::api::service::{ApiError, BoxFuture, ErrorCode, RequestCtx};

/// The port key of [`GraphPort`].
pub const KEY: PortKey = PortKey::of::<dyn GraphPort>();

/// The port key of [`LineagePort`] (a second port in this file: the registry lists the first).
pub const LINEAGE_KEY: PortKey = PortKey::of::<dyn LineagePort>();

/// What a port call answers.
pub type GraphResult<T> = Result<T, ApiError>;

/// The deepest `graph_query` walk the contract allows.
pub const GRAPH_MAX_DEPTH: u64 = 8;
/// The most nodes a `graph_query` may ask for.
pub const GRAPH_MAX_LIMIT: u64 = 1_000;
/// The most edge types a `graph_query` may name.
pub const GRAPH_MAX_EDGE_TYPES: usize = 16;
/// The deepest a filter may nest.
pub const GRAPH_MAX_FILTER_DEPTH: usize = 4;
/// The most terms a filter may hold in all.
pub const GRAPH_MAX_FILTER_TERMS: usize = 32;
/// The deepest `lineage_trace` walk the contract allows.
pub const LINEAGE_MAX_DEPTH: u64 = 12;
/// The most nodes a `lineage_trace` may ask for.
pub const LINEAGE_MAX_LIMIT: u64 = 5_000;

/// A record named by DocType and id only (the contract's `GraphNodeRef`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeRef {
    /// The DocType's storage name.
    pub doctype: String,
    /// The record id (a canonical UUIDv7).
    pub id: String,
}

impl NodeRef {
    /// A node reference.
    pub fn new(doctype: impl Into<String>, id: impl Into<String>) -> Self {
        NodeRef {
            doctype: doctype.into(),
            id: id.into(),
        }
    }
}

/// The direction of a walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectionIn {
    /// Follow edges away from the start.
    Out,
    /// Follow edges towards the start.
    In,
    /// Follow both.
    Both,
}

/// A literal in a filter. A decimal travels as text (ADR-12), never as a number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiteralIn {
    /// A string of at most 1,024 characters.
    Text(String),
    /// A boolean.
    Bool(bool),
    /// A whole number.
    Int(i64),
}

/// A comparison operator of a filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOpIn {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
}

/// A filter over the fields of one record: the vocabulary of `spot-graph-query`'s filter and no
/// other. The route has checked its depth and term count before a port sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterIn {
    /// `{field, op, value}`.
    Compare {
        /// The field's storage name.
        field: String,
        /// The operator.
        op: CompareOpIn,
        /// The literal compared with.
        value: LiteralIn,
    },
    /// `{field, in: [..]}`.
    In {
        /// The field's storage name.
        field: String,
        /// The candidates.
        values: Vec<LiteralIn>,
    },
    /// `{field, is_null}`.
    IsNull {
        /// The field's storage name.
        field: String,
        /// Whether the field must be null (`true`) or not null (`false`).
        is_null: bool,
    },
    /// All of the terms.
    And(Vec<FilterIn>),
    /// Any of the terms.
    Or(Vec<FilterIn>),
    /// The term does not hold.
    Not(Box<FilterIn>),
}

/// The body of `graph_query`, already validated by the route (the contract's `GraphQuery`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphQueryIn {
    /// The record the walk starts from.
    pub start: NodeRef,
    /// The direction to follow.
    pub direction: DirectionIn,
    /// The edge types to follow; empty means every type the caller may traverse.
    pub edge_types: Vec<String>,
    /// The deepest level, 1 to 8.
    pub depth: u8,
    /// A filter over the caller's view of each node.
    pub node_filter: Option<FilterIn>,
    /// A filter over the properties of the edge crossed.
    pub edge_filter: Option<FilterIn>,
    /// Whether to return the readable properties of an edge.
    pub return_edge_properties: bool,
    /// The most nodes to return, 1 to 1,000.
    pub limit: u32,
}

/// One property value of an edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgePropertyOut {
    /// Text (a decimal is text, ADR-12).
    Text(String),
    /// A boolean.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// No value (a field the caller may read that is null).
    Null,
}

/// One edge of an answer (the contract's `GraphEdge`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeOut {
    /// The record the edge leaves.
    pub from: NodeRef,
    /// The record the edge reaches.
    pub to: NodeRef,
    /// The edge type's storage name.
    pub kind: String,
    /// The relation's properties the caller may read; `None` unless both ends are visible and
    /// the query asked for them.
    pub properties: Option<BTreeMap<String, EdgePropertyOut>>,
}

/// Why a graph answer is partial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphTruncationOut {
    /// The depth was reached with more to find.
    Depth,
    /// The node limit was reached.
    Nodes,
    /// The path budget was reached.
    Paths,
}

/// The answer of `graph_query` (the contract's `GraphAnswer`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphAnswerOut {
    /// The nodes the caller may see.
    pub nodes: Vec<NodeRef>,
    /// The edges the caller may see.
    pub edges: Vec<EdgeOut>,
    /// Whether the answer is the part found within the budget.
    pub truncated: bool,
    /// Why, when `truncated`.
    pub truncation_reason: Option<GraphTruncationOut>,
    /// Whether something the caller may not read was removed. Never how many or what.
    pub withheld: bool,
}

/// The graph of one site, for the signed-in caller.
pub trait GraphPort: Send + Sync {
    /// Walks the graph from `query.start` as the caller `ctx` names. A start record that does
    /// not exist and one the caller may not read answer the same `graph.not_found`.
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a GraphQueryIn,
    ) -> BoxFuture<'a, GraphResult<GraphAnswerOut>>;
}

/// The named lineage questions (the contract's `LineageKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineageKindIn {
    /// The whole document chain around a record.
    Chain,
    /// A batch followed to where it went.
    TraceForward,
    /// A batch followed to where it came from.
    TraceBack,
    /// A bill of materials exploded, with exact quantities.
    Bom,
    /// The products an item goes into.
    WhereUsed,
}

impl LineageKindIn {
    /// The kind's wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            LineageKindIn::Chain => "chain",
            LineageKindIn::TraceForward => "trace_forward",
            LineageKindIn::TraceBack => "trace_back",
            LineageKindIn::Bom => "bom",
            LineageKindIn::WhereUsed => "where_used",
        }
    }
}

/// The body of `lineage_trace`, already validated by the route (the contract's
/// `LineageRequest`). The chain configuration is server data: there is no field for one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageQueryIn {
    /// The question.
    pub kind: LineageKindIn,
    /// The record the question starts from.
    pub start: NodeRef,
    /// The deepest level, 1 to 12; `None` lets the port use its default.
    pub depth: Option<u8>,
    /// The most nodes, 1 to 5,000; `None` lets the port use its default.
    pub limit: Option<u32>,
}

/// One node of a lineage level (the contract's `LineageNode`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageNodeOut {
    /// The record.
    pub node: NodeRef,
    /// The exact decimal quantity (a `bom` answer only), as text.
    pub quantity: Option<String>,
}

/// One level of a lineage answer (the contract's `LineageLevel`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageLevelOut {
    /// The distance from the start record; 0 is the start.
    pub depth: u8,
    /// The nodes of the level.
    pub nodes: Vec<LineageNodeOut>,
    /// The number of nodes of the level.
    pub node_count: u32,
    /// The exact sum of the level's quantities, as text (a `bom` answer only).
    pub quantity_total: Option<String>,
}

/// Why a lineage answer is partial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineageTruncationOut {
    /// The depth was reached with more to find.
    Depth,
    /// The node limit was reached.
    Nodes,
}

/// The answer of `lineage_trace` (the contract's `LineageAnswer`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageAnswerOut {
    /// The levels, from the start record outwards.
    pub levels: Vec<LineageLevelOut>,
    /// Whether the answer is the part found within the budget.
    pub truncated: bool,
    /// Why, when `truncated`.
    pub truncation_reason: Option<LineageTruncationOut>,
    /// Whether a record the caller may not read was removed. Never how many or what.
    pub withheld: bool,
}

/// The lineage questions of one site, for the signed-in caller.
pub trait LineagePort: Send + Sync {
    /// Answers `query` as the caller `ctx` names. A start record that does not exist and one the
    /// caller may not read answer the same `graph.not_found`.
    fn trace<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a LineageQueryIn,
    ) -> BoxFuture<'a, GraphResult<LineageAnswerOut>>;
}

/// The code of an operation whose service this site does not run.
pub const NOT_CONFIGURED: &str = "service.not_configured";

/// The error a port answers with when its service is not configured on the site. The routes
/// turn it into `501 service.not_configured`; it is not one of the contract's refusals, so a
/// port uses it only through this function.
pub fn not_configured() -> ApiError {
    ApiError::app(NOT_CONFIGURED, "This site has no graph service configured.")
}

/// Whether `error` is [`not_configured`].
pub fn is_not_configured(error: &ApiError) -> bool {
    matches!(&error.code, ErrorCode::App(code) if code == NOT_CONFIGURED)
}

/// A port for a site that runs one of the two services and not the other: every call answers
/// [`not_configured`]. A host mounts the group with it in place of the missing adapter, so the
/// operation answers `501 service.not_configured` instead of not being served at all.
pub struct Unconfigured;

impl GraphPort for Unconfigured {
    fn query<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        _query: &'a GraphQueryIn,
    ) -> BoxFuture<'a, GraphResult<GraphAnswerOut>> {
        Box::pin(async { Err(not_configured()) })
    }
}

impl LineagePort for Unconfigured {
    fn trace<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        _query: &'a LineageQueryIn,
    ) -> BoxFuture<'a, GraphResult<LineageAnswerOut>> {
        Box::pin(async { Err(not_configured()) })
    }
}

fn not_found() -> ApiError {
    ApiError::from_code(Code::GraphNotFound)
}

fn token_of(ctx: &RequestCtx) -> Option<&str> {
    ctx.credential.as_ref().map(|c| c.token.as_str())
}

// ---------------------------------------------------------------------------
// The graph fake

/// What a token may see on one site.
#[derive(Clone, Debug)]
enum Visible {
    All,
    Only(HashSet<NodeRef>),
    AllBut(HashSet<NodeRef>),
}

impl Visible {
    fn sees(&self, node: &NodeRef) -> bool {
        match self {
            Visible::All => true,
            Visible::Only(set) => set.contains(node),
            Visible::AllBut(set) => !set.contains(node),
        }
    }
}

#[derive(Clone, Debug)]
struct StoredEdge {
    from: NodeRef,
    to: NodeRef,
    kind: String,
    properties: BTreeMap<String, EdgePropertyOut>,
}

#[derive(Default)]
struct GraphSite {
    nodes: BTreeSet<NodeRef>,
    edges: Vec<StoredEdge>,
    tokens: HashMap<String, Visible>,
}

#[derive(Default)]
struct GraphInner {
    sites: HashMap<SiteId, GraphSite>,
    /// The queries the port received, oldest first.
    queries: Vec<GraphQueryIn>,
    /// When set, the next call fails with it.
    fail_next: Option<ApiError>,
}

/// An in-memory [`GraphPort`] over a small fixed graph per site, with a visibility set per
/// token. It decides nothing a real adapter decides beyond what is described here: it walks
/// breadth first over the nodes the token sees, ignores the filters (a test reads them back
/// through [`MemoryGraph::queries`]) and returns edge properties only when the query asks and
/// both ends are visible.
#[derive(Default)]
pub struct MemoryGraph {
    inner: Mutex<GraphInner>,
}

impl MemoryGraph {
    /// An empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GraphInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Adds a node to `site`'s graph.
    pub fn add_node(&self, site: &SiteId, node: NodeRef) {
        self.lock()
            .sites
            .entry(site.clone())
            .or_default()
            .nodes
            .insert(node);
    }

    /// Adds an edge (and its two ends) to `site`'s graph.
    pub fn add_edge(&self, site: &SiteId, from: NodeRef, to: NodeRef, kind: &str) {
        self.add_edge_with(site, from, to, kind, BTreeMap::new());
    }

    /// Adds an edge with properties.
    pub fn add_edge_with(
        &self,
        site: &SiteId,
        from: NodeRef,
        to: NodeRef,
        kind: &str,
        properties: BTreeMap<String, EdgePropertyOut>,
    ) {
        let mut inner = self.lock();
        let entry = inner.sites.entry(site.clone()).or_default();
        entry.nodes.insert(from.clone());
        entry.nodes.insert(to.clone());
        entry.edges.push(StoredEdge {
            from,
            to,
            kind: kind.to_owned(),
            properties,
        });
    }

    /// Lets `token` see every node of `site`.
    pub fn grant_all(&self, site: &SiteId, token: &str) {
        self.lock()
            .sites
            .entry(site.clone())
            .or_default()
            .tokens
            .insert(token.to_owned(), Visible::All);
    }

    /// Lets `token` see every node of `site` except `hidden`.
    pub fn grant_all_but(&self, site: &SiteId, token: &str, hidden: &[NodeRef]) {
        self.lock()
            .sites
            .entry(site.clone())
            .or_default()
            .tokens
            .insert(
                token.to_owned(),
                Visible::AllBut(hidden.iter().cloned().collect()),
            );
    }

    /// Lets `token` see exactly `visible` on `site`.
    pub fn grant_only(&self, site: &SiteId, token: &str, visible: &[NodeRef]) {
        self.lock()
            .sites
            .entry(site.clone())
            .or_default()
            .tokens
            .insert(
                token.to_owned(),
                Visible::Only(visible.iter().cloned().collect()),
            );
    }

    /// Makes the next call fail with `error`.
    pub fn fail_next(&self, error: ApiError) {
        self.lock().fail_next = Some(error);
    }

    /// The queries the port received, oldest first.
    pub fn queries(&self) -> Vec<GraphQueryIn> {
        self.lock().queries.clone()
    }

    fn run(&self, ctx: &RequestCtx, query: &GraphQueryIn) -> GraphResult<GraphAnswerOut> {
        let mut inner = self.lock();
        inner.queries.push(query.clone());
        if let Some(error) = inner.fail_next.take() {
            return Err(error);
        }
        let token = token_of(ctx).ok_or_else(not_found)?;
        let site = inner.sites.get(&ctx.site).ok_or_else(not_found)?;
        let visible = site.tokens.get(token).ok_or_else(not_found)?;
        if !site.nodes.contains(&query.start) || !visible.sees(&query.start) {
            return Err(not_found());
        }
        Ok(walk(site, visible, query))
    }
}

impl GraphPort for MemoryGraph {
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a GraphQueryIn,
    ) -> BoxFuture<'a, GraphResult<GraphAnswerOut>> {
        Box::pin(async move { self.run(ctx, query) })
    }
}

/// The edges of `site` that leave `node` under the query's direction and edge types, as
/// `(edge, the node at the other end)`, in insertion order.
fn neighbours<'a>(
    site: &'a GraphSite,
    node: &NodeRef,
    query: &GraphQueryIn,
) -> Vec<(&'a StoredEdge, &'a NodeRef)> {
    let mut out = Vec::new();
    for edge in &site.edges {
        if !query.edge_types.is_empty() && !query.edge_types.contains(&edge.kind) {
            continue;
        }
        let outgoing = edge.from == *node;
        let incoming = edge.to == *node;
        match query.direction {
            DirectionIn::Out if outgoing => out.push((edge, &edge.to)),
            DirectionIn::In if incoming => out.push((edge, &edge.from)),
            DirectionIn::Both if outgoing => out.push((edge, &edge.to)),
            DirectionIn::Both if incoming => out.push((edge, &edge.from)),
            _ => {}
        }
    }
    out
}

fn walk(site: &GraphSite, visible: &Visible, query: &GraphQueryIn) -> GraphAnswerOut {
    let limit = usize::try_from(query.limit).unwrap_or(usize::MAX);
    let mut nodes: Vec<NodeRef> = vec![query.start.clone()];
    let mut seen: HashSet<NodeRef> = HashSet::from([query.start.clone()]);
    let mut edges: Vec<EdgeOut> = Vec::new();
    let mut seen_edges: HashSet<(NodeRef, NodeRef, String)> = HashSet::new();
    let mut withheld = false;
    let mut truncated = None;
    let mut queue: VecDeque<(NodeRef, u8)> = VecDeque::from([(query.start.clone(), 0)]);
    'walk: while let Some((node, level)) = queue.pop_front() {
        for (edge, other) in neighbours(site, &node, query) {
            if !visible.sees(other) {
                // Removed with every path through it; only the flag says so.
                withheld = true;
                continue;
            }
            if level >= query.depth {
                // More to find beyond the depth that was asked for.
                if !seen.contains(other) && truncated.is_none() {
                    truncated = Some(GraphTruncationOut::Depth);
                }
                continue;
            }
            if !seen.contains(other) {
                if nodes.len() >= limit {
                    truncated = Some(GraphTruncationOut::Nodes);
                    break 'walk;
                }
                seen.insert(other.clone());
                nodes.push(other.clone());
                queue.push_back((other.clone(), level + 1));
            }
            let key = (edge.from.clone(), edge.to.clone(), edge.kind.clone());
            if seen_edges.insert(key) {
                edges.push(EdgeOut {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    kind: edge.kind.clone(),
                    properties: (query.return_edge_properties && !edge.properties.is_empty())
                        .then(|| edge.properties.clone()),
                });
            }
        }
    }
    GraphAnswerOut {
        nodes,
        edges,
        truncated: truncated.is_some(),
        truncation_reason: truncated,
        withheld,
    }
}

// ---------------------------------------------------------------------------
// The lineage fake

/// One stored lineage node: its place in the tree decides what removes it.
#[derive(Clone, Debug)]
struct LineageSpec {
    node: NodeRef,
    quantity: Option<String>,
    /// The index of the parent in the stored list; `None` for the start record.
    parent: Option<usize>,
    depth: u8,
}

#[derive(Default)]
struct LineageSite {
    /// By start record: the tree under it, parents before children, the start first.
    trees: HashMap<NodeRef, Vec<LineageSpec>>,
    tokens: HashMap<String, Visible>,
}

#[derive(Default)]
struct LineageInner {
    sites: HashMap<SiteId, LineageSite>,
    queries: Vec<LineageQueryIn>,
    fail_next: Option<ApiError>,
}

/// An in-memory [`LineagePort`]: a stored tree per start record, whatever the kind asked (the
/// kind is recorded, see [`MemoryLineage::queries`]), cut by depth and limit and by the token's
/// visibility. A node the token cannot see is removed with its whole subtree.
#[derive(Default)]
pub struct MemoryLineage {
    inner: Mutex<LineageInner>,
}

impl MemoryLineage {
    /// An empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LineageInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stores the tree of `start` on `site`. `children` are `(parent index, node, quantity)`;
    /// index 0 is the start itself, the first child is index 1, and a parent must come before
    /// its children.
    pub fn add_tree(
        &self,
        site: &SiteId,
        start: NodeRef,
        quantity: Option<&str>,
        children: &[(usize, NodeRef, Option<&str>)],
    ) {
        let mut specs = vec![LineageSpec {
            node: start.clone(),
            quantity: quantity.map(str::to_owned),
            parent: None,
            depth: 0,
        }];
        for (parent, node, quantity) in children {
            let depth = specs.get(*parent).map_or(1, |p| p.depth.saturating_add(1));
            specs.push(LineageSpec {
                node: node.clone(),
                quantity: quantity.map(str::to_owned),
                parent: Some(*parent),
                depth,
            });
        }
        self.lock()
            .sites
            .entry(site.clone())
            .or_default()
            .trees
            .insert(start, specs);
    }

    /// Lets `token` see every node of `site`.
    pub fn grant_all(&self, site: &SiteId, token: &str) {
        self.lock()
            .sites
            .entry(site.clone())
            .or_default()
            .tokens
            .insert(token.to_owned(), Visible::All);
    }

    /// Lets `token` see every node of `site` except `hidden`.
    pub fn grant_all_but(&self, site: &SiteId, token: &str, hidden: &[NodeRef]) {
        self.lock()
            .sites
            .entry(site.clone())
            .or_default()
            .tokens
            .insert(
                token.to_owned(),
                Visible::AllBut(hidden.iter().cloned().collect()),
            );
    }

    /// Makes the next call fail with `error`.
    pub fn fail_next(&self, error: ApiError) {
        self.lock().fail_next = Some(error);
    }

    /// The queries the port received, oldest first.
    pub fn queries(&self) -> Vec<LineageQueryIn> {
        self.lock().queries.clone()
    }

    fn run(&self, ctx: &RequestCtx, query: &LineageQueryIn) -> GraphResult<LineageAnswerOut> {
        let mut inner = self.lock();
        inner.queries.push(query.clone());
        if let Some(error) = inner.fail_next.take() {
            return Err(error);
        }
        let token = token_of(ctx).ok_or_else(not_found)?;
        let site = inner.sites.get(&ctx.site).ok_or_else(not_found)?;
        let visible = site.tokens.get(token).ok_or_else(not_found)?;
        let tree = site.trees.get(&query.start).ok_or_else(not_found)?;
        if !visible.sees(&query.start) {
            return Err(not_found());
        }
        Ok(cut(tree, visible, query))
    }
}

impl LineagePort for MemoryLineage {
    fn trace<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a LineageQueryIn,
    ) -> BoxFuture<'a, GraphResult<LineageAnswerOut>> {
        Box::pin(async move { self.run(ctx, query) })
    }
}

fn cut(tree: &[LineageSpec], visible: &Visible, query: &LineageQueryIn) -> LineageAnswerOut {
    let max_depth = query.depth.unwrap_or(12);
    let limit = query
        .limit
        .and_then(|l| usize::try_from(l).ok())
        .unwrap_or(usize::MAX);
    // `kept[i]`: node i survives visibility (it and every ancestor is visible).
    let mut kept = vec![false; tree.len()];
    let mut withheld = false;
    for (i, spec) in tree.iter().enumerate() {
        let parent_kept = spec.parent.is_none_or(|p| kept[p]);
        if !visible.sees(&spec.node) {
            withheld = true;
        } else if parent_kept {
            kept[i] = true;
        } else {
            // Visible, but only reachable through a node the caller cannot see.
            withheld = true;
        }
    }
    let mut levels: BTreeMap<u8, Vec<LineageNodeOut>> = BTreeMap::new();
    let mut truncated = None;
    let mut count = 0usize;
    for (i, spec) in tree.iter().enumerate() {
        if !kept[i] {
            continue;
        }
        if spec.depth > max_depth {
            truncated.get_or_insert(LineageTruncationOut::Depth);
            continue;
        }
        if count >= limit {
            truncated = Some(LineageTruncationOut::Nodes);
            break;
        }
        count += 1;
        levels.entry(spec.depth).or_default().push(LineageNodeOut {
            node: spec.node.clone(),
            quantity: spec.quantity.clone(),
        });
    }
    let levels = levels
        .into_iter()
        .map(|(depth, nodes)| {
            let quantity_total = if query.kind == LineageKindIn::Bom {
                nodes
                    .iter()
                    .map(|n| {
                        n.quantity
                            .as_deref()
                            .and_then(|q| q.parse::<Decimal>().ok())
                    })
                    .try_fold(Decimal::ZERO, |acc, q| q.and_then(|q| acc.checked_add(q)))
                    .map(|total| total.to_string())
            } else {
                None
            };
            LineageLevelOut {
                depth,
                node_count: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
                nodes,
                quantity_total,
            }
        })
        .collect();
    LineageAnswerOut {
        levels,
        truncated: truncated.is_some(),
        truncation_reason: truncated,
        withheld,
    }
}
