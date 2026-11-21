//! The effect badges of the designer: per node, what it declares and how it is undone.

use spot_app_api::Reversal;
use spot_bpm_model::{NodeId, ProcessVersion};
use spot_seams::Registry;

use crate::declared::{Source, node_effects};
use crate::problem::{Undo, Where};

/// One declared effect with its reversibility.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EffectBadge {
    /// The step that declares it (`<app>:<step>`).
    pub step: String,
    /// The class.
    pub class: String,
    /// Internal or external.
    pub kind: Where,
    /// How it is undone, in the contract's summary.
    pub reversibility: Undo,
    /// The lifecycle operation that reverses it (`ReversibleBy`) or the compensating action
    /// (`CompensableBy`), as text; absent for an irreversible effect.
    pub by: Option<String>,
}

/// The declared effects of one node.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeEffects {
    /// The node.
    pub node: NodeId,
    /// Its effects, in the order its pipeline runs them.
    pub effects: Vec<EffectBadge>,
}

/// The declared effects of every node that declares any, in node-id order. Deterministic: it does
/// not depend on the order of the nodes in the process.
#[must_use]
pub fn effect_summary(process: &ProcessVersion, registry: &Registry) -> Vec<NodeEffects> {
    summary_with(process, registry)
}

pub(crate) fn summary_with(process: &ProcessVersion, source: &dyn Source) -> Vec<NodeEffects> {
    let mut out: Vec<NodeEffects> = process
        .nodes
        .iter()
        .filter_map(|node| {
            let effects: Vec<EffectBadge> = node_effects(source, node)
                .into_iter()
                .map(|d| EffectBadge {
                    step: d.step.to_string(),
                    class: d.decl.class.as_str().to_owned(),
                    kind: d.decl.kind.into(),
                    reversibility: d.decl.reversal.reversibility().into(),
                    by: match &d.decl.reversal {
                        Reversal::ReversibleBy(op) => {
                            Some(spot_app_api::lifecycle::op_name(*op).to_owned())
                        }
                        Reversal::CompensableBy(action) => Some(action.to_string()),
                        _ => None,
                    },
                })
                .collect();
            (!effects.is_empty()).then(|| NodeEffects {
                node: node.id.clone(),
                effects,
            })
        })
        .collect();
    out.sort();
    out
}
