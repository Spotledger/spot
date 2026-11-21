//! The per-source builders.
//!
//! Each adapter reads one authoritative source and returns nodes, edges and history;
//! [`crate::SelfModel::build`] folds them into one snapshot. An adapter is a pure read: time
//! is carried in every record by the caller.

pub mod apps;
pub mod health;
pub mod metadata;
pub mod migrations;
pub mod permissions;
pub mod scripts;
pub mod seams;
pub mod settings;

pub use apps::AppsAdapter;
pub use health::{FleetFinding, HealthAdapter, HealthSignalRecord};
pub use metadata::MetadataAdapter;
pub use migrations::{AppInstallRecord, MigrationImpact, MigrationsAdapter, PatchRecord};
pub use permissions::{AssignmentRecord, PermissionsAdapter};
pub use scripts::{ScriptRecord, ScriptsAdapter};
pub use seams::SeamsAdapter;
pub use settings::{SettingRecord, SettingsAdapter};

use crate::SelfModelError;
use crate::evidence::{EvidenceRef, EvidenceSource, Provenance};
use crate::kind::EdgeKind;
use crate::model::{Edge, Node, NodeRef, SourceData};

/// The canonical name of a pipeline step: DocType, hook point and step.
#[must_use]
pub(crate) fn pipeline_key(doctype: &str, hook_point: &str, step: &str) -> String {
    format!("{doctype}.{hook_point}.{step}")
}

/// Adds a node, merging a second contribution from the same source.
pub(crate) fn push_node(nodes: &mut Vec<Node>, node: Node) {
    if let Some(existing) = nodes.iter_mut().find(|seen| seen.id == node.id) {
        for (key, value) in node.attributes {
            existing.attributes.entry(key).or_insert(value);
        }
        existing.evidence.extend(node.evidence);
        existing.evidence.sort();
        existing.evidence.dedup();
        return;
    }
    nodes.push(node);
}

/// Adds an edge, merging evidence from a second contribution.
pub(crate) fn push_edge(edges: &mut Vec<Edge>, edge: Edge) {
    if let Some(existing) = edges.iter_mut().find(|seen| seen.id == edge.id) {
        existing.evidence.extend(edge.evidence);
        existing.evidence.sort();
        existing.evidence.dedup();
        existing.at = match (existing.at, edge.at) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (None, right) => right,
            (left, None) => left,
        };
        return;
    }
    edges.push(edge);
}

/// Adds a placeholder node for a node another source owns, so an edge always has both ends.
pub(crate) fn ensure_node(
    data: &mut SourceData,
    reference: &NodeRef,
    evidence: &EvidenceRef,
) -> Result<(), SelfModelError> {
    let node = Node::new(
        reference.kind,
        reference.name.clone(),
        vec![evidence.clone()],
    )?;
    push_node(&mut data.nodes, node);
    Ok(())
}

/// Adds the `changed_by` edge a provenance record names, when it names a cause.
///
/// The cause node is created when this source has not seen it; another source that owns it
/// contributes the rest of its attributes on merge.
pub(crate) fn push_provenance_cause(
    data: &mut SourceData,
    subject: &NodeRef,
    provenance: &Provenance,
    source: EvidenceSource,
) -> Result<(), SelfModelError> {
    let Some(cause) = &provenance.cause else {
        return Ok(());
    };
    let evidence = provenance.evidence(source);
    ensure_node(data, cause, &evidence)?;
    push_edge(
        &mut data.edges,
        Edge::new(
            subject.id(),
            cause.id(),
            EdgeKind::ChangedBy,
            Some(provenance.at),
            vec![evidence],
        )?,
    );
    Ok(())
}
