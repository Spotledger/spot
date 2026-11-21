//! The drift verifier: does the projection match its source?
//!
//! [`verify`] compares a sink against the same [`GraphSource`] the rebuild reads, and lists every
//! disagreement as a [`Drift`] — a missing or extra node or edge, a property that differs, or a
//! watermark that is behind or ahead. An empty list means **the projection matches the source
//! graph** (the WP-GRA-07 acceptance invariant). The comparison mirrors the in-memory index's
//! checker, so a hot index and an external sink are held to the same standard.

use std::collections::{BTreeMap, BTreeSet};

use spot_graph_index::{Drift, EdgeKey, GraphSource};
use spot_storage_contract::{NodeRef, Value};

use crate::error::ProjectionError;
use crate::sink::SinkReader;

/// Compares `sink` against `source`: empty means the projection equals the source graph,
/// including the watermark. The result is deterministic (key order).
///
/// # Errors
///
/// [`ProjectionError::Source`] when the source cannot answer.
pub fn verify(
    source: &dyn GraphSource,
    sink: &dyn SinkReader,
) -> Result<Vec<Drift>, ProjectionError> {
    let key = sink.key();
    let expected = source.stream_edges(&key).map_err(ProjectionError::from)?;
    let watermark = source.watermark(&key).map_err(ProjectionError::from)?;

    let mut expected_edges: BTreeMap<EdgeKey, BTreeMap<String, Value>> = BTreeMap::new();
    let mut expected_nodes: BTreeSet<NodeRef> = BTreeSet::new();
    for edge in expected {
        expected_nodes.insert(edge.key.from.clone());
        expected_nodes.insert(edge.key.to.clone());
        expected_edges.insert(edge.key, edge.properties);
    }

    let mut actual_edges: BTreeMap<EdgeKey, BTreeMap<String, Value>> = BTreeMap::new();
    let mut actual_nodes: BTreeSet<NodeRef> = BTreeSet::new();
    for edge in sink.edges() {
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
    if sink.watermark() != watermark {
        drifts.push(Drift::WatermarkMismatch {
            index: sink.watermark(),
            source: watermark,
        });
    }
    Ok(drifts)
}
