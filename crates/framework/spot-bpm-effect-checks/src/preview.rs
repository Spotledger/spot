//! The preview of a designer test run: what each node produced, before go-live.

use std::collections::BTreeMap;

use spot_app_api::OperationReceipt;
use spot_bpm_model::NodeId;
use spot_seams::{Registry, UndeclaredReason, check_receipt};

use crate::error::{ChecksError, Result};
use crate::problem::Where;

/// The most receipts one preview takes.
pub const MAX_RECEIPTS: usize = 10_000;
/// The most effects one receipt may carry (the engine's own bound).
pub const MAX_EFFECTS_PER_RECEIPT: usize = 256;

/// The receipt one node's operation returned during a test run.
#[derive(Clone, Debug)]
pub struct TestRunReceipt {
    /// The node that ran the operation.
    pub node: NodeId,
    /// The receipt.
    pub receipt: OperationReceipt,
}

/// One effect the run produced. Names and counts only: no reference, digest or value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PreviewEffect {
    /// The operation that produced it, by name.
    pub operation: String,
    /// The DocType.
    pub doctype: String,
    /// The step.
    pub step: String,
    /// The class.
    pub class: String,
    /// Internal or external.
    pub kind: Where,
    /// How many rows or messages.
    pub count: u64,
    /// Whether nothing the registry declares covers it.
    pub undeclared: Option<UndeclaredReason>,
}

/// What one node produced.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodePreview {
    /// The node.
    pub node: NodeId,
    /// Its effects, in the order the receipts reported them.
    pub effects: Vec<PreviewEffect>,
}

/// The total of one class and kind over the whole run.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClassTotal {
    /// The class.
    pub class: String,
    /// Internal or external.
    pub kind: Where,
    /// How many effect entries.
    pub effects: u64,
    /// The sum of their counts (saturating).
    pub count: u64,
}

/// What the designer shows before go-live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// Per node, in node-id order; nodes without effects are listed with an empty list.
    pub nodes: Vec<NodePreview>,
    /// Per class and kind, sorted.
    pub totals: Vec<ClassTotal>,
    /// How many effects no declaration covers: a conformance failure to fix before go-live.
    pub undeclared: u64,
}

/// Groups the receipts of a test run by node, flags the effects no declaration covers and totals
/// per class.
///
/// Receipts of one node keep their given order, so the result depends on the order of the input
/// only through that. A receipt with an effect list beyond the engine's bound, or more than
/// [`MAX_RECEIPTS`] receipts, is refused whole.
///
/// # Errors
/// [`ChecksError::TooLarge`] beyond the bounds.
pub fn preview_from_receipts(registry: &Registry, receipts: &[TestRunReceipt]) -> Result<Preview> {
    if receipts.len() > MAX_RECEIPTS {
        return Err(ChecksError::TooLarge {
            what: "test run receipts",
            found: receipts.len(),
            limit: MAX_RECEIPTS,
        });
    }
    let mut by_node: BTreeMap<&NodeId, Vec<PreviewEffect>> = BTreeMap::new();
    let mut totals: BTreeMap<(String, Where), (u64, u64)> = BTreeMap::new();
    let mut undeclared_total = 0u64;
    for run in receipts {
        let receipt = &run.receipt;
        if receipt.effects.len() > MAX_EFFECTS_PER_RECEIPT {
            return Err(ChecksError::TooLarge {
                what: "effects in a receipt",
                found: receipt.effects.len(),
                limit: MAX_EFFECTS_PER_RECEIPT,
            });
        }
        let missing = check_receipt(registry, &receipt.doctype, receipt.operation, receipt);
        let entry = by_node.entry(&run.node).or_default();
        for effect in &receipt.effects {
            let reason = missing
                .iter()
                .find(|u| u.step == effect.step && u.class == effect.class && u.kind == effect.kind)
                .map(|u| u.reason);
            if reason.is_some() {
                undeclared_total = undeclared_total.saturating_add(1);
            }
            let count = u64::from(effect.count);
            let slot = totals
                .entry((effect.class.as_str().to_owned(), Where::from(effect.kind)))
                .or_default();
            slot.0 = slot.0.saturating_add(1);
            slot.1 = slot.1.saturating_add(count);
            entry.push(PreviewEffect {
                operation: spot_app_api::lifecycle::op_name(receipt.operation).to_owned(),
                doctype: receipt.doctype.clone(),
                step: effect.step.to_string(),
                class: effect.class.as_str().to_owned(),
                kind: effect.kind.into(),
                count,
                undeclared: reason,
            });
        }
    }
    Ok(Preview {
        nodes: by_node
            .into_iter()
            .map(|(node, effects)| NodePreview {
                node: node.clone(),
                effects,
            })
            .collect(),
        totals: totals
            .into_iter()
            .map(|((class, kind), (effects, count))| ClassTotal {
                class,
                kind,
                effects,
                count,
            })
            .collect(),
        undeclared: undeclared_total,
    })
}
