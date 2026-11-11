//! The query seam: a [`GraphExecutor`] that answers the walk from the index when the graph is
//! hot, and from the database path otherwise.
//!
//! The index holds the graph's **structure** (which records are connected by which edge types).
//! A record's field values still come from the database path, so filters and permissions see
//! exactly what storage would give them. The index only shortens the traversal:
//!
//! - **hot**: [`GraphIndex::step`] answers each step;
//! - **cold or stale**: the wrapped fallback source answers, so a missing or stale graph never
//!   serves a read.
//!
//! This is the `GraphExecutor` implementation `spot_graph_query::contract::same_answers` runs
//! against the storage executor: give both the same records and the answers are equal.

use std::sync::Arc;

use spot_graph_model::GraphModel;
use spot_graph_query::{
    EdgeFieldAccess, EdgeProps, GraphExecutor, GraphQuery, GraphQueryError, GraphResult,
    Invocation, NodeData, NodeSource, Step, compile_with, walk,
};
use spot_storage_contract::{Name, NodeRef};

use crate::index::GraphIndex;

/// Runs graph queries from a hot index, falling back to `S` for values, for a cold graph and for
/// a stale one.
pub struct IndexExecutor<S> {
    source: S,
    model: Arc<GraphModel>,
    index: Option<Arc<GraphIndex>>,
    edge_access: Arc<dyn EdgeFieldAccess>,
}

impl<S> IndexExecutor<S> {
    /// An executor over `source` (the database path), following `model`'s edge types, with the
    /// given hot graph (or `None` when cold), under the caller's field-level access to edge
    /// properties. The policy is required, exactly as for the storage executor, so the hot and
    /// the cold path hide the same properties: bind it to the caller's principal and rules.
    pub fn new(
        source: S,
        model: Arc<GraphModel>,
        index: Option<Arc<GraphIndex>>,
        edge_access: Arc<dyn EdgeFieldAccess>,
    ) -> Self {
        Self {
            source,
            model,
            index,
            edge_access,
        }
    }

    /// An executor with a hot graph.
    pub fn hot(
        source: S,
        model: Arc<GraphModel>,
        index: Arc<GraphIndex>,
        edge_access: Arc<dyn EdgeFieldAccess>,
    ) -> Self {
        Self::new(source, model, Some(index), edge_access)
    }

    /// An executor with no hot graph: every step goes to the database path.
    pub fn cold(source: S, model: Arc<GraphModel>, edge_access: Arc<dyn EdgeFieldAccess>) -> Self {
        Self::new(source, model, None, edge_access)
    }

    /// Swaps the hot graph, as a store's `get` or `rebuild` would.
    pub fn set_index(&mut self, index: Option<Arc<GraphIndex>>) {
        self.index = index;
    }

    /// The hot graph, when one is set.
    #[must_use]
    pub fn index(&self) -> Option<&Arc<GraphIndex>> {
        self.index.as_ref()
    }

    /// The database-path source back, for commit or reuse.
    pub fn into_source(self) -> S {
        self.source
    }
}

impl<S: NodeSource> GraphExecutor for IndexExecutor<S> {
    async fn execute(
        &mut self,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> Result<GraphResult, GraphQueryError> {
        let steps = compile_with(&self.model, query, self.edge_access.as_ref())?;
        // A stale graph must not answer: treat it exactly like a cold one.
        let index = self.index.as_deref().filter(|graph| !graph.is_stale());
        let mut source = IndexSource {
            fallback: &mut self.source,
            index,
        };
        walk(&mut source, &steps, query, who).await
    }
}

/// Whether the step follows a relation edge type that merges the repeated lines of a pair
/// (`aggregate`). The merged edge is the sum of the lines **the caller may read**; the index holds
/// one edge per pair and cannot tell which lines a caller may read, so such a step always goes to
/// the database path, which merges through [`spot_graph_query::merge_lines`]. Hot and cold
/// therefore give the same answer by construction.
fn merges_lines(step: &Step) -> bool {
    step.relation
        .as_ref()
        .is_some_and(|relation| relation.aggregate_sum.is_some())
}

/// The [`NodeSource`] the walk reads: values from the fallback, steps from the index when hot.
struct IndexSource<'a, S> {
    fallback: &'a mut S,
    index: Option<&'a GraphIndex>,
}

impl<S: NodeSource> NodeSource for IndexSource<'_, S> {
    async fn load(&mut self, node: &NodeRef) -> Result<Option<NodeData>, GraphQueryError> {
        self.fallback.load(node).await
    }

    async fn step(
        &mut self,
        node: &NodeRef,
        step: &Step,
        max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        match self.index.filter(|_| !merges_lines(step)) {
            Some(index) => index.step(node, step, max_nodes),
            None => self.fallback.step(node, step, max_nodes).await,
        }
    }

    async fn edge_properties(
        &mut self,
        from: &NodeRef,
        to: &NodeRef,
        step: &Step,
    ) -> Result<Option<EdgeProps>, GraphQueryError> {
        let Some(index) = self.index.filter(|_| !merges_lines(step)) else {
            return self.fallback.edge_properties(from, to, step).await;
        };
        // Only the declared properties the caller may read leave the graph (`step.properties`
        // excludes the hidden ones); the walk drops the hidden ones once more.
        Ok(index.crossed_edge_properties(from, to, step).map(|stored| {
            stored
                .iter()
                .filter_map(|(name, value)| Name::new(name).ok().map(|n| (n, value.clone())))
                .filter(|(name, _)| step.properties.is_empty() || step.properties.contains(name))
                .collect()
        }))
    }
}
