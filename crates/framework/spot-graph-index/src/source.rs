//! The rebuild source and the index-equals-database invariant checker.
//!
//! [`GraphSource`] is the database side of a rebuild: it streams a graph's current edges and
//! reports the watermark the index should carry afterwards. [`check_equal`] compares a built
//! index against the same source and lists every [`Drift`], so "the index equals the database"
//! is a check a test (or a repair job) can run, not a claim.

use std::collections::{BTreeMap, BTreeSet};

use spot_storage_contract::{NodeRef, Value};

use crate::error::GraphIndexError;
use crate::event::{Edge, EdgeKey};
use crate::index::GraphIndex;
use crate::key::GraphKey;

/// The database side of a graph: the edges it currently holds, and its watermark.
///
/// `stream_edges` returns the graph's current edges in storage orientation; `watermark` is the
/// event sequence the database has reached, which a rebuilt index adopts so the next event is
/// `watermark + 1`.
pub trait GraphSource {
    /// Every edge of `key`, in any order; duplicates collapse in the index.
    ///
    /// # Errors
    ///
    /// [`GraphIndexError::Source`] when the database cannot answer.
    fn stream_edges(&self, key: &GraphKey) -> Result<Vec<Edge>, GraphIndexError>;

    /// The event sequence `key` has reached.
    ///
    /// # Errors
    ///
    /// [`GraphIndexError::Source`] when the database cannot answer.
    fn watermark(&self, key: &GraphKey) -> Result<u64, GraphIndexError>;
}

/// One way an index and its source disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Drift {
    /// The source has an edge endpoint the index does not.
    MissingNode(NodeRef),
    /// The index has an edge endpoint the source does not.
    ExtraNode(NodeRef),
    /// The source has an edge the index does not.
    MissingEdge(EdgeKey),
    /// The index has an edge the source does not.
    ExtraEdge(EdgeKey),
    /// Both have the edge, with different properties.
    PropertyMismatch {
        /// The edge.
        edge: EdgeKey,
        /// The index's properties.
        index: BTreeMap<String, Value>,
        /// The source's properties.
        source: BTreeMap<String, Value>,
    },
    /// The index's watermark differs from the source's.
    WatermarkMismatch {
        /// The index's watermark.
        index: u64,
        /// The source's watermark.
        source: u64,
    },
}

impl Drift {
    /// The drift's stable text tag, for reports.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MissingNode(_) => "missing_node",
            Self::ExtraNode(_) => "extra_node",
            Self::MissingEdge(_) => "missing_edge",
            Self::ExtraEdge(_) => "extra_edge",
            Self::PropertyMismatch { .. } => "property_mismatch",
            Self::WatermarkMismatch { .. } => "watermark_mismatch",
        }
    }
}

/// Compares `index` against `source`: empty means the index equals the database, including the
/// watermark. The result is deterministic (key order).
///
/// # Errors
///
/// Whatever the source fails with.
pub fn check_equal(
    index: &GraphIndex,
    source: &dyn GraphSource,
) -> Result<Vec<Drift>, GraphIndexError> {
    let key = index.key().clone();
    let edges = source.stream_edges(&key)?;
    let watermark = source.watermark(&key)?;

    let mut expected_edges: BTreeMap<EdgeKey, BTreeMap<String, Value>> = BTreeMap::new();
    let mut expected_nodes: BTreeSet<NodeRef> = BTreeSet::new();
    for edge in edges {
        expected_nodes.insert(edge.key.from.clone());
        expected_nodes.insert(edge.key.to.clone());
        expected_edges.insert(edge.key, edge.properties);
    }
    let mut actual_edges: BTreeMap<EdgeKey, BTreeMap<String, Value>> = BTreeMap::new();
    let mut actual_nodes: BTreeSet<NodeRef> = BTreeSet::new();
    for edge in index.edges() {
        actual_nodes.insert(edge.key.from.clone());
        actual_nodes.insert(edge.key.to.clone());
        actual_edges.insert(edge.key, edge.properties);
    }

    let mut drifts: Vec<Drift> = Vec::new();
    for node in expected_nodes.difference(&actual_nodes) {
        drifts.push(Drift::MissingNode(node.clone()));
    }
    for node in actual_nodes.difference(&expected_nodes) {
        drifts.push(Drift::ExtraNode(node.clone()));
    }
    for (edge, properties) in &expected_edges {
        match actual_edges.get(edge) {
            None => drifts.push(Drift::MissingEdge(edge.clone())),
            Some(found) if found != properties => drifts.push(Drift::PropertyMismatch {
                edge: edge.clone(),
                index: found.clone(),
                source: properties.clone(),
            }),
            Some(_) => {}
        }
    }
    for edge in actual_edges.keys() {
        if !expected_edges.contains_key(edge) {
            drifts.push(Drift::ExtraEdge(edge.clone()));
        }
    }
    if index.watermark() != watermark {
        drifts.push(Drift::WatermarkMismatch {
            index: index.watermark(),
            source: watermark,
        });
    }
    Ok(drifts)
}
