//! Which effects a process node declares, read from the registry.

use spot_app_api::{HookPoint, Operation, Phase, Target};
use spot_bpm_model::{DocumentAction, Node, NodeKind};
use spot_seams::{DeclaredEffect, Registry};

/// Where declarations come from. The registry implements it; the checks' own unit tests use a
/// table for shapes a built registry rejects.
pub(crate) trait Source {
    fn effects(&self, doctype: &str, operation: Operation) -> Vec<DeclaredEffect>;

    /// Whether the registry plans any step for `operation` on `doctype`. False means the
    /// (doctype, operation) pair is unknown to the registry (or has no steps at all), so an
    /// empty effect list says nothing about what it does.
    fn has_pipeline(&self, doctype: &str, operation: Operation) -> bool;
}

impl Source for Registry {
    fn effects(&self, doctype: &str, operation: Operation) -> Vec<DeclaredEffect> {
        Registry::effects(self, doctype, operation)
    }

    fn has_pipeline(&self, doctype: &str, operation: Operation) -> bool {
        [
            Phase::Before,
            Phase::Validate,
            Phase::After,
            Phase::Committed,
        ]
        .into_iter()
        .any(|phase| {
            !self
                .plan(doctype, &HookPoint::of(phase, Target::Op(operation)))
                .is_empty()
        }) || !Registry::effects(self, doctype, operation).is_empty()
    }
}

/// The lifecycle operation a document node runs, when it writes one. `map` copies fields and
/// writes nothing; an operation name the contract does not know is skipped (the model's own
/// validation reports it).
pub(crate) fn node_operation(node: &Node) -> Option<(&str, Operation)> {
    let NodeKind::Document(config) = &node.kind else {
        return None;
    };
    if config.action == DocumentAction::Map {
        return None;
    }
    let op = parse_operation(config.operation())?;
    Some((config.doctype.as_str(), op))
}

/// The declared effects of a node, in pipeline order.
pub(crate) fn node_effects(source: &dyn Source, node: &Node) -> Vec<DeclaredEffect> {
    node_operation(node)
        .map(|(doctype, op)| source.effects(doctype, op))
        .unwrap_or_default()
}

pub(crate) fn parse_operation(name: &str) -> Option<Operation> {
    Some(match name {
        "create" => Operation::Create,
        "update" => Operation::Update,
        "post" => Operation::Post,
        "cancel" => Operation::Cancel,
        "delete" => Operation::Delete,
        "rename" => Operation::Rename,
        "update_posted" => Operation::UpdatePosted,
        _ => return None,
    })
}
