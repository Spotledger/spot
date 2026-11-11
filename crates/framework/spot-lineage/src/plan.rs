//! Compiling a [`LineageQuery`] into graph queries.
//!
//! Each question compiles to a [`Plan`]: the edge types to follow at every level, the direction,
//! how many levels to walk, the node budget, the fields to return, and — for a bill-of-materials
//! explosion — the quantity field and the basis. [`Plan::level_query`] turns one level into a
//! [`GraphQuery`], so a lineage query is a sequence of graph queries that never bypass the graph
//! query API's permission filter.

use spot_graph_model::GraphModel;
use spot_graph_query::{
    DEFAULT_MAX_NODES, Direction, EdgeSelector, GraphQuery, Include, MAX_DEPTH, MAX_NODES_CEILING,
    Order, Step, compile as compile_query,
};
use spot_storage_contract::{Name, NodeRef};

use crate::config::ChainConfig;
use crate::error::LineageError;
use crate::quantity::Quantity;
use crate::query::LineageQuery;

/// The bill-of-materials part of a plan: the field carrying the per-unit quantity and the
/// quantity the trace starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BomPlan {
    /// The field on the component record carrying the per-unit quantity.
    pub qty_field: Name,
    /// The quantity the trace starts from.
    pub basis: Quantity,
}

/// A lineage query compiled to the shape of the graph queries that answer it.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    query: LineageQuery,
    follow: Vec<EdgeSelector>,
    direction: Direction,
    levels: u16,
    max_nodes: u32,
    fields: Vec<Name>,
    bom: Option<BomPlan>,
}

/// Compiles `query` against `config` into a plan.
///
/// # Errors
///
/// [`LineageError::UnknownChain`] or [`LineageError::UnknownBom`] when the configuration has no
/// entry for the query's DocType, and [`LineageError::DepthOutOfRange`] for a depth outside the
/// graph query API's range.
pub fn compile(query: &LineageQuery, config: &ChainConfig) -> Result<Plan, LineageError> {
    match query {
        LineageQuery::Document {
            doctype,
            direction,
            max_depth,
            ..
        } => {
            check_depth(*max_depth)?;
            let chain = config
                .chain(doctype)
                .ok_or_else(|| LineageError::UnknownChain {
                    doctype: doctype.as_str().to_owned(),
                })?;
            Ok(Plan {
                query: query.clone(),
                follow: directed(&chain.edges, *direction),
                direction: *direction,
                levels: *max_depth,
                max_nodes: DEFAULT_MAX_NODES,
                fields: Vec::new(),
                bom: None,
            })
        }
        LineageQuery::BatchTrace { batch, direction } => {
            let chain = config
                .chain(&batch.entity)
                .ok_or_else(|| LineageError::UnknownChain {
                    doctype: batch.entity.as_str().to_owned(),
                })?;
            Ok(Plan {
                query: query.clone(),
                follow: directed(&chain.edges, *direction),
                direction: *direction,
                levels: MAX_DEPTH,
                max_nodes: DEFAULT_MAX_NODES,
                fields: Vec::new(),
                bom: None,
            })
        }
        LineageQuery::BomExplosion {
            item,
            qty_basis,
            max_depth,
        } => {
            check_depth(*max_depth)?;
            let bom = config
                .bom(&item.entity)
                .ok_or_else(|| LineageError::UnknownBom {
                    doctype: item.entity.as_str().to_owned(),
                })?;
            Ok(Plan {
                query: query.clone(),
                follow: vec![EdgeSelector::directed(bom.edge.clone(), Direction::In)],
                direction: Direction::In,
                levels: *max_depth,
                max_nodes: DEFAULT_MAX_NODES,
                fields: vec![bom.qty_field.clone()],
                bom: Some(BomPlan {
                    qty_field: bom.qty_field.clone(),
                    basis: *qty_basis,
                }),
            })
        }
        LineageQuery::WhereUsed { item } => {
            let bom = config
                .bom(&item.entity)
                .ok_or_else(|| LineageError::UnknownBom {
                    doctype: item.entity.as_str().to_owned(),
                })?;
            Ok(Plan {
                query: query.clone(),
                follow: vec![EdgeSelector::directed(bom.edge.clone(), Direction::Out)],
                direction: Direction::Out,
                levels: MAX_DEPTH,
                max_nodes: DEFAULT_MAX_NODES,
                fields: Vec::new(),
                bom: None,
            })
        }
    }
}

fn directed(edges: &[String], direction: Direction) -> Vec<EdgeSelector> {
    edges
        .iter()
        .map(|edge| EdgeSelector::directed(edge.clone(), direction))
        .collect()
}

fn check_depth(depth: u16) -> Result<(), LineageError> {
    if (1..=MAX_DEPTH).contains(&depth) {
        Ok(())
    } else {
        Err(LineageError::DepthOutOfRange {
            depth,
            max: MAX_DEPTH,
        })
    }
}

impl Plan {
    /// The query this plan answers.
    #[must_use]
    pub fn query(&self) -> &LineageQuery {
        &self.query
    }

    /// How many levels the walk has.
    #[must_use]
    pub fn levels(&self) -> u16 {
        self.levels
    }

    /// The edge types followed at each level.
    #[must_use]
    pub fn follow(&self) -> &[EdgeSelector] {
        &self.follow
    }

    /// The direction the edges are followed in.
    #[must_use]
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The fields returned on every node.
    #[must_use]
    pub fn fields(&self) -> &[Name] {
        &self.fields
    }

    /// The node budget.
    #[must_use]
    pub fn max_nodes(&self) -> u32 {
        self.max_nodes
    }

    /// The bill-of-materials part, for an explosion.
    #[must_use]
    pub fn bom(&self) -> Option<&BomPlan> {
        self.bom.as_ref()
    }

    /// The start records.
    #[must_use]
    pub fn start(&self) -> Vec<NodeRef> {
        match &self.query {
            LineageQuery::Document { doctype, name, .. } => vec![NodeRef {
                entity: doctype.clone(),
                id: *name,
            }],
            LineageQuery::BatchTrace { batch, .. } => vec![batch.clone()],
            LineageQuery::BomExplosion { item, .. } | LineageQuery::WhereUsed { item } => {
                vec![item.clone()]
            }
        }
    }

    /// The same plan with another node budget.
    ///
    /// # Errors
    ///
    /// [`LineageError::NodeBudgetOutOfRange`] when `max_nodes` is 0 or above the graph query
    /// API's ceiling.
    pub fn with_max_nodes(mut self, max_nodes: u32) -> Result<Self, LineageError> {
        if !(1..=MAX_NODES_CEILING).contains(&max_nodes) {
            return Err(LineageError::NodeBudgetOutOfRange {
                max_nodes,
                max: MAX_NODES_CEILING,
            });
        }
        self.max_nodes = max_nodes;
        Ok(self)
    }

    /// The graph query for one level, starting from `from`, following every edge type of the
    /// plan once.
    ///
    /// # Errors
    ///
    /// Whatever [`GraphQuery::builder`] reports: no start node, or a node budget outside the
    /// graph query API's range.
    pub fn level_query(
        &self,
        from: Vec<NodeRef>,
        max_nodes: u32,
    ) -> Result<GraphQuery, LineageError> {
        let query = GraphQuery::builder()
            .from(from)
            .follow(self.follow.clone())
            .direction(self.direction)
            .max_depth(1)
            .max_nodes(max_nodes)
            .include(Include {
                fields: self.fields.clone(),
                edge_properties: false,
            })
            .order(Order::Breadth)
            .build()?;
        Ok(query)
    }

    /// The plan's edge types compiled to storage steps against `model`, for callers that inspect
    /// the compiled shape and for the [`crate::oracle`].
    ///
    /// # Errors
    ///
    /// Whatever the graph query API's compiler reports, as [`LineageError::Graph`].
    pub fn steps(&self, model: &GraphModel) -> Result<Vec<Step>, LineageError> {
        let query = self.level_query(self.start(), self.max_nodes)?;
        Ok(compile_query(model, &query)?)
    }
}
