//! The traversal state, derived from the graph model (WP-STO-07.T5, gap D1 of 2026-11-02).
//!
//! "Traversable" is not a flag on a field. A Link field is followed incoming by a graph
//! traversal exactly when `spot-graph-model` derives an edge over it, so the model is the single
//! source of truth: [`state_from_model`] reads it and gives the [`TraversalState`] that
//! [`crate::plan_with_traversal`] takes. The index and the traversal can never disagree, and no
//! metadata schema changes.
//!
//! # What counts
//!
//! - A `Link` field of a non-child DocType: the edge `<doctype>.<field>`, contributing
//!   `(doctype, field)`.
//! - A relation edge: the relation DocType's two endpoint fields, contributing
//!   `(relation doctype, from_field)` and `(relation doctype, to_field)`.
//!
//! Not counted: `Table` containment edges (no Link column), `TableMultiselect` and `DynamicLink`
//! edges (no single reverse column on the owner), and any field the model flags with a
//! dangling target (a link to a DocType that is not there is not traversable).

use std::collections::BTreeSet;

use spot_graph_model::{EdgeSource, GraphModel, Problem};

use crate::traversal::{TraversalLink, TraversalState};

/// The traversal state of `model`: the sorted, de-duplicated Link fields that an edge follows.
///
/// `indexed` (the Link fields whose reverse index exists) and `reserved_names` (index names the
/// catalogue uses for other purposes) are facts of the live system, not of the model, so they are
/// passed through unchanged. Deterministic: the same model gives the same state.
#[must_use]
pub fn state_from_model(
    model: &GraphModel,
    indexed: &[TraversalLink],
    reserved_names: &[String],
) -> TraversalState {
    let dangling: BTreeSet<(&str, &str)> = model
        .problems()
        .iter()
        .filter_map(|problem| match problem {
            Problem::DanglingTarget { doctype, field, .. } => {
                Some((doctype.as_str(), field.as_str()))
            }
            _ => None,
        })
        .collect();

    let mut links: BTreeSet<TraversalLink> = BTreeSet::new();
    for edge in model.edges.values() {
        match edge.source {
            EdgeSource::LinkField => {
                let field = edge
                    .name
                    .strip_prefix(edge.from.as_str())
                    .and_then(|rest| rest.strip_prefix('.'));
                if let Some(field) = field {
                    links.insert(TraversalLink::new(edge.from.as_str(), field));
                }
            }
            EdgeSource::Relation => {
                if let Some(relation) = &edge.relation {
                    links.insert(TraversalLink::new(
                        relation.doctype.as_str(),
                        relation.from_field.as_str(),
                    ));
                    links.insert(TraversalLink::new(
                        relation.doctype.as_str(),
                        relation.to_field.as_str(),
                    ));
                }
            }
            EdgeSource::Table | EdgeSource::TableMultiselect | EdgeSource::DynamicLink => {}
        }
    }

    TraversalState {
        traversable: links
            .into_iter()
            .filter(|link| !dangling.contains(&(link.doctype.as_str(), link.field.as_str())))
            .collect(),
        indexed: indexed.to_vec(),
        reserved_names: reserved_names.to_vec(),
    }
}
