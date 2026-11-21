//! The graph query: what to walk, how far, and what to return. Built with [`GraphQuery::builder`];
//! [`GraphQueryBuilder::build`] validates the whole query and reports every problem at once.

use spot_storage_contract::{Expr, Name, NodeRef};

use crate::error::{GraphQueryError, QueryProblem, problem};
use crate::filter;

/// The largest depth a query may ask for (storage's limit).
pub const MAX_DEPTH: u16 = 64;
/// The default node budget of a query.
pub const DEFAULT_MAX_NODES: u32 = 1_000;
/// The largest node budget a query may ask for.
pub const MAX_NODES_CEILING: u32 = 100_000;

/// The default bound on the paths a summed rollup evaluates.
pub const DEFAULT_MAX_PATHS: u32 = 10_000;
/// The largest bound on the paths a summed rollup evaluates; a larger request is clamped.
pub const MAX_PATHS_CEILING: u32 = 1_000_000;

/// Which way an edge type is followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Direction {
    /// From the record holding the link to the record it points at.
    #[default]
    Out,
    /// From a record to the records whose link points at it.
    In,
    /// Both ways.
    Both,
}

/// One edge type to follow, by its canonical name in the graph model, with an optional direction
/// that overrides the query's default for this edge type only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeSelector {
    /// The canonical edge-type name, for example `invoice.grn`.
    pub edge: String,
    /// The direction for this edge type, when it differs from the query's.
    pub direction: Option<Direction>,
}

impl EdgeSelector {
    /// Follows `edge` in the query's default direction.
    #[must_use]
    pub fn new(edge: impl Into<String>) -> Self {
        Self {
            edge: edge.into(),
            direction: None,
        }
    }

    /// Follows `edge` in `direction`, whatever the query's default is.
    #[must_use]
    pub fn directed(edge: impl Into<String>, direction: Direction) -> Self {
        Self {
            edge: edge.into(),
            direction: Some(direction),
        }
    }
}

/// What a result carries besides the nodes and edges.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Include {
    /// The fields to return per node, by name. Empty returns no fields.
    pub fields: Vec<Name>,
    /// Returns, per node, the path from the start node ([`crate::GraphNode::path`]) with the
    /// properties of every edge on it. Edges whose source holds no properties (link, table and
    /// dynamic-link edges in storage) give empty maps.
    pub edge_properties: bool,
}

/// The order of the result's nodes and edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Order {
    /// By depth, then entity, then record id: the order storage returns (ties on the record id
    /// break by its binary value).
    #[default]
    Breadth,
    /// By entity, then record id.
    ByEntityThenId,
}

/// A validated graph query. Fields are public for reading; build with [`GraphQuery::builder`].
#[derive(Clone, Debug, PartialEq)]
pub struct GraphQuery {
    /// Start records. Missing or unreadable ones contribute nothing.
    pub from: Vec<NodeRef>,
    /// The edge types to follow, at least one.
    pub follow: Vec<EdgeSelector>,
    /// The default direction.
    pub direction: Direction,
    /// Deepest level returned, 1 to [`MAX_DEPTH`].
    pub max_depth: u16,
    /// The node budget: more reached nodes fail with `storage.traversal_too_large`.
    pub max_nodes: u32,
    /// Keeps a node only when true. Evaluated on the node's own values.
    pub node_filter: Option<Expr>,
    /// Keeps an edge only when true. Evaluated on the values of the record holding the link.
    pub edge_filter: Option<Expr>,
    /// What to return besides the structure.
    pub include: Include,
    /// The result order.
    pub order: Order,
    /// The edge variable (`-[e:BOM_ITEM]->`): a field reference whose alias is this name speaks
    /// about the properties of the edge being crossed, not about the record holding the link.
    pub edge_var: Option<Name>,
    /// The numeric edge property to multiply along each path (a BOM's rolled-up quantity); see
    /// [`crate::GraphNode::rolled_up`].
    pub rollup: Option<Name>,
    /// Whether the rollup is the sum over all simple paths of the product along each path
    /// ([`GraphQueryBuilder::rollup_sum`]) instead of the product along the walk's one path.
    pub rollup_sum: bool,
    /// The bound on the paths a summed rollup evaluates, 1 to [`MAX_PATHS_CEILING`].
    pub max_paths: u32,
    /// Whether a larger `max_paths` was asked for and clamped to [`MAX_PATHS_CEILING`]; the
    /// result says so ([`crate::GraphResult::max_paths_clamped`]).
    pub max_paths_clamped: bool,
}

impl GraphQuery {
    /// An empty builder with the defaults: outgoing, depth 1, the default node budget.
    #[must_use]
    pub fn builder() -> GraphQueryBuilder {
        GraphQueryBuilder::default()
    }

    /// Descendants of `node` along `edge`: the records that link to it, transitively, up to
    /// `depth`. For a child table, the rows of the document.
    ///
    /// # Errors
    ///
    /// The builder's problems, as [`GraphQueryError::Invalid`].
    pub fn descendants(
        node: NodeRef,
        edge: &str,
        depth: u16,
    ) -> Result<GraphQuery, GraphQueryError> {
        Self::builder()
            .from(vec![node])
            .follow(vec![EdgeSelector::new(edge)])
            .direction(Direction::In)
            .max_depth(depth)
            .build()
    }

    /// Ancestors of `node` along `edge`: the records it links to, transitively, up to `depth`.
    ///
    /// # Errors
    ///
    /// The builder's problems, as [`GraphQueryError::Invalid`].
    pub fn ancestors(node: NodeRef, edge: &str, depth: u16) -> Result<GraphQuery, GraphQueryError> {
        Self::builder()
            .from(vec![node])
            .follow(vec![EdgeSelector::new(edge)])
            .direction(Direction::Out)
            .max_depth(depth)
            .build()
    }

    /// The records one step away from `node` along any of `edges`, in either direction.
    ///
    /// # Errors
    ///
    /// The builder's problems, as [`GraphQueryError::Invalid`].
    pub fn neighbours(node: NodeRef, edges: &[&str]) -> Result<GraphQuery, GraphQueryError> {
        Self::builder()
            .from(vec![node])
            .follow(edges.iter().map(|e| EdgeSelector::new(*e)).collect())
            .direction(Direction::Both)
            .max_depth(1)
            .build()
    }

    /// Everything connected to `node` along `edges` in either direction, to the deepest level:
    /// the purchase-order, receipt, invoice and payment chain in one call.
    ///
    /// # Errors
    ///
    /// The builder's problems, as [`GraphQueryError::Invalid`].
    pub fn lineage(node: NodeRef, edges: &[&str]) -> Result<GraphQuery, GraphQueryError> {
        Self::builder()
            .from(vec![node])
            .follow(edges.iter().map(|e| EdgeSelector::new(*e)).collect())
            .direction(Direction::Both)
            .max_depth(MAX_DEPTH)
            .build()
    }
}

/// Collects a query's parts. Nothing is checked until [`GraphQueryBuilder::build`].
#[derive(Clone, Debug, Default)]
pub struct GraphQueryBuilder {
    from: Vec<NodeRef>,
    follow: Vec<EdgeSelector>,
    direction: Direction,
    max_depth: Option<u16>,
    max_nodes: Option<u32>,
    node_filter: Option<Expr>,
    edge_filter: Option<Expr>,
    include: Include,
    order: Order,
    edge_var: Option<Name>,
    rollup: Option<Name>,
    rollup_sum: bool,
    max_paths: Option<u32>,
}

impl GraphQueryBuilder {
    /// The start records.
    #[must_use]
    pub fn from(mut self, nodes: Vec<NodeRef>) -> Self {
        self.from = nodes;
        self
    }

    /// The edge types to follow.
    #[must_use]
    pub fn follow(mut self, edges: Vec<EdgeSelector>) -> Self {
        self.follow = edges;
        self
    }

    /// The default direction.
    #[must_use]
    pub fn direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }

    /// The deepest level, 1 to [`MAX_DEPTH`]. Defaults to 1.
    #[must_use]
    pub fn max_depth(mut self, depth: u16) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// The node budget, 1 to [`MAX_NODES_CEILING`]. Defaults to [`DEFAULT_MAX_NODES`].
    #[must_use]
    pub fn max_nodes(mut self, nodes: u32) -> Self {
        self.max_nodes = Some(nodes);
        self
    }

    /// Keeps a node only when `expr` holds for it.
    #[must_use]
    pub fn node_filter(mut self, expr: Expr) -> Self {
        self.node_filter = Some(expr);
        self
    }

    /// Keeps an edge only when `expr` holds for the record holding the link.
    #[must_use]
    pub fn edge_filter(mut self, expr: Expr) -> Self {
        self.edge_filter = Some(expr);
        self
    }

    /// What to return besides the structure.
    #[must_use]
    pub fn include(mut self, include: Include) -> Self {
        self.include = include;
        self
    }

    /// The result order.
    #[must_use]
    pub fn order(mut self, order: Order) -> Self {
        self.order = order;
        self
    }

    /// Names the edge variable: a field reference with this alias in the edge filter reads the
    /// properties of the crossed edge (`e.qty >= 2`).
    #[must_use]
    pub fn edge_var(mut self, name: Name) -> Self {
        self.edge_var = Some(name);
        self
    }

    /// Multiplies the numeric edge property `property` along each path and returns the product on
    /// every node ([`crate::GraphNode::rolled_up`]).
    #[must_use]
    pub fn rollup(mut self, property: Name) -> Self {
        self.rollup = Some(property);
        self
    }

    /// Sums the numeric edge property over all simple paths: for each reached node, the sum over
    /// every simple path from a start node of the product of `property` along that path (a BOM
    /// explosion: a part used through two sub-assemblies gets the sum of the path products). The
    /// result is [`crate::GraphNode::rolled_up`], and [`crate::GraphNode::path_count`] says how
    /// many paths contributed. Bounded by [`GraphQueryBuilder::max_paths`].
    #[must_use]
    pub fn rollup_sum(mut self, property: Name) -> Self {
        self.rollup = Some(property);
        self.rollup_sum = true;
        self
    }

    /// The bound on the paths a summed rollup evaluates. Defaults to [`DEFAULT_MAX_PATHS`]; a
    /// value above [`MAX_PATHS_CEILING`] is clamped to it (and the result says so), and 0 is
    /// raised to 1.
    #[must_use]
    pub fn max_paths(mut self, paths: u32) -> Self {
        self.max_paths = Some(paths);
        self
    }

    /// Validates the query, reporting every problem found.
    ///
    /// # Errors
    ///
    /// [`GraphQueryError::Invalid`] with one entry per problem: no start node, no edge type, a
    /// depth or node budget out of range, or a filter outside the supported subset.
    pub fn build(self) -> Result<GraphQuery, GraphQueryError> {
        let mut problems: Vec<QueryProblem> = Vec::new();
        if self.from.is_empty() {
            problems.push(problem("graphq.no_start", "the query names no start node"));
        }
        if self.follow.is_empty() {
            problems.push(problem("graphq.no_edges", "the query follows no edge type"));
        }
        let max_depth = self.max_depth.unwrap_or(1);
        if !(1..=MAX_DEPTH).contains(&max_depth) {
            problems.push(problem(
                "graphq.depth_out_of_range",
                format!("max_depth must be 1 to {MAX_DEPTH}, not {max_depth}"),
            ));
        }
        let max_nodes = self.max_nodes.unwrap_or(DEFAULT_MAX_NODES);
        if !(1..=MAX_NODES_CEILING).contains(&max_nodes) {
            problems.push(problem(
                "graphq.max_nodes_out_of_range",
                format!("max_nodes must be 1 to {MAX_NODES_CEILING}, not {max_nodes}"),
            ));
        }
        for expr in [&self.node_filter, &self.edge_filter].into_iter().flatten() {
            if let Err(p) = filter::validate(expr) {
                problems.push(p);
            }
        }
        if !problems.is_empty() {
            return Err(GraphQueryError::Invalid(problems));
        }
        let asked_paths = self.max_paths.unwrap_or(DEFAULT_MAX_PATHS);
        Ok(GraphQuery {
            max_paths: asked_paths.clamp(1, MAX_PATHS_CEILING),
            max_paths_clamped: asked_paths > MAX_PATHS_CEILING,
            rollup_sum: self.rollup_sum,
            from: self.from,
            follow: self.follow,
            direction: self.direction,
            max_depth,
            max_nodes,
            node_filter: self.node_filter,
            edge_filter: self.edge_filter,
            include: self.include,
            order: self.order,
            edge_var: self.edge_var,
            rollup: self.rollup,
        })
    }
}
