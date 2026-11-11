//! Where a receipt's reversals come from: the effect declarations of the record type.
//!
//! A receipt says what an operation did; the **declaration** says how each effect is undone
//! (`ReversibleBy`, `CompensableBy`, `Irreversible`). The file derives an entry's reversals once,
//! when the entry is recorded, and keeps them, so a later change to a declaration never changes
//! what an old entry will do. An effect with no matching declaration is a point of no return:
//! the undo fails closed rather than guess.

use std::collections::BTreeMap;

use spot_app_api::Operation;
use spot_app_api::effects::reversing_operation;
use spot_app_api::{OperationReceipt, Reversal};
use spot_seams::{DeclaredEffect, Registry};

use crate::model::EntryReversal;

/// The declared effects of an operation on a DocType.
pub trait Declarations: Send + Sync {
    /// Every effect any step of the operation's pipeline declares.
    fn effects(&self, doctype: &str, operation: Operation) -> Vec<DeclaredEffect>;
}

impl Declarations for Registry {
    fn effects(&self, doctype: &str, operation: Operation) -> Vec<DeclaredEffect> {
        Registry::effects(self, doctype, operation)
    }
}

/// A fixed table of declarations, for hosts and tests that do not run a registry.
#[derive(Clone, Debug, Default)]
pub struct StaticDeclarations {
    table: BTreeMap<String, Vec<(Operation, DeclaredEffect)>>,
}

impl StaticDeclarations {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares one effect for an operation on a DocType.
    #[must_use]
    pub fn with(mut self, doctype: &str, operation: Operation, effect: DeclaredEffect) -> Self {
        self.table
            .entry(doctype.to_owned())
            .or_default()
            .push((operation, effect));
        self
    }
}

impl Declarations for StaticDeclarations {
    fn effects(&self, doctype: &str, operation: Operation) -> Vec<DeclaredEffect> {
        self.table
            .get(doctype)
            .map(|rows| {
                rows.iter()
                    .filter(|(op, _)| *op == operation)
                    .map(|(_, e)| e.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The declared ways to undo what `receipt` records, without duplicates.
///
/// Each reported effect takes the reversal of the declaration that matches its step, class and
/// kind; an effect no declaration covers is a point of no return. A `post` always carries its
/// pair (`cancel`), because the record itself must be reversed whatever its effects say. With no
/// effects, the operation decides: `create` and `update` touch only a draft and have nothing to
/// reverse; `cancel`, `delete`, `rename` and `update_posted` have no reversal and fail closed.
#[must_use]
pub fn derive_reversals(
    declarations: &dyn Declarations,
    receipt: &OperationReceipt,
) -> Vec<EntryReversal> {
    let declared = declarations.effects(&receipt.doctype, receipt.operation);
    let mut out: Vec<EntryReversal> = Vec::new();
    let mut push = |r: EntryReversal| {
        if !out.contains(&r) {
            out.push(r);
        }
    };
    if let Some(pair) = reversing_operation(receipt.operation) {
        push(EntryReversal::Lifecycle(pair));
    }
    for effect in &receipt.effects {
        let found = declared.iter().find(|d| {
            d.step == effect.step && d.decl.class == effect.class && d.decl.kind == effect.kind
        });
        push(match found.map(|d| &d.decl.reversal) {
            Some(Reversal::ReversibleBy(op)) => EntryReversal::Lifecycle(*op),
            Some(Reversal::CompensableBy(action)) => EntryReversal::Action(action.clone()),
            _ => EntryReversal::PointOfNoReturn,
        });
    }
    if receipt.effects.is_empty() {
        match receipt.operation {
            Operation::Create | Operation::Update | Operation::Post => {}
            _ => push(EntryReversal::PointOfNoReturn),
        }
    }
    out.sort_by_key(|r| match r {
        EntryReversal::Lifecycle(_) => 0u8,
        EntryReversal::Action(_) => 1,
        EntryReversal::PointOfNoReturn => 2,
    });
    out
}
