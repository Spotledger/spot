//! Effect declarations in the registry (WP-EXT-11.T2; ADR-080 sections 6.3 and 6.4).
//!
//! A controller declares what its steps do to the world through
//! [`Controller::step_effects`](spot_app_api::Controller::step_effects) (contract 0.5.0). The
//! registry keeps those declarations beside each planned step, so the process engine can ask
//! what an operation will do ([`Registry::effects`]) and so a receipt can be checked against
//! it ([`check_receipt`]).
//!
//! Three things are checked when the registry is built (all reported as
//! [`Conflict`](crate::Conflict)s with the other install-time checks):
//!
//! - each declaration on its own and against the hook point of its step (the contract's own
//!   rules, for example `effects.internal_not_reversible`), and, when the builder was given the
//!   set of known effect classes, `effects.unknown_class`;
//! - pairing: an internal effect at `after_post` that is reversible by `cancel` needs an
//!   internal effect at `after_cancel` writing the same stores, or the cancel pipeline does not
//!   undo what post did (`effects.no_paired_reversal`);
//! - overrides: when the pairing holds without overrides but an override removes the reversing
//!   step while the forward step remains (`effects.override_drops_reversal`).
//!
//! Declarations never change the run order or the seam and override rules.

use std::collections::BTreeSet;

use spot_app_api::effects::reversing_operation;
use spot_app_api::lifecycle::sequence;
use spot_app_api::{
    EffectClassName, EffectDecl, EffectKind, HookPoint, Operation, OperationReceipt, Phase, Stage,
    StepName, StoreName, Target,
};

use crate::registry::{PlannedStep, Registry};

/// One effect a step in an operation's pipeline declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredEffect {
    /// The step that declares it.
    pub step: StepName,
    /// The app that declared the step.
    pub app: String,
    /// The hook point of the pipeline it runs in.
    pub point: HookPoint,
    /// The declaration.
    pub decl: EffectDecl,
}

/// Why a reported effect is not covered by a declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UndeclaredReason {
    /// The receipt is for another DocType or operation than the one checked. Every effect in it
    /// is reported, because none can be matched to this pipeline.
    ReceiptMismatch,
    /// The step is not in the pipeline or declares no effects at all.
    NoDeclaration,
    /// The step declares effects, but none of this class.
    ClassNotDeclared,
    /// The step declares this class, but as the other kind (internal or external).
    KindMismatch,
}

/// An effect in a receipt that no declaration covers (ADR-080 6.4): a conformance failure in the
/// test kit and a logged event in production. It carries names only; the opaque references and
/// the digest of the receipt are not copied, so it is safe to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undeclared {
    /// The step the receipt names.
    pub step: StepName,
    /// The class the receipt names.
    pub class: EffectClassName,
    /// The kind the receipt names.
    pub kind: EffectKind,
    /// Why nothing covers it.
    pub reason: UndeclaredReason,
}

/// How an override changed the effects of the step it targets (ADR-080 6.3, the admin listing).
/// Only overrides that change effects get a row; one that leaves the effects as they were does
/// not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectChange {
    /// The app that declared the override.
    pub app: String,
    /// The override, as the report names it: `replace`, `wrap`, `disable` or `reorder`.
    pub kind: crate::OverrideRowKind,
    /// The step it changes, as `app:step`.
    pub target_step: String,
    /// The DocType of the step.
    pub doctype: String,
    /// The hook point where the override takes effect (the ADR-51 name).
    pub hook_point: String,
    /// Effects that no longer happen.
    pub removed: Vec<EffectDecl>,
    /// Effects that now also or instead happen.
    pub added: Vec<EffectDecl>,
}

/// Every lifecycle operation, for walking the pairs. A list, not a match, so a new operation
/// cannot break this crate's build.
pub(crate) const OPERATIONS: [Operation; 7] = [
    Operation::Create,
    Operation::Update,
    Operation::Post,
    Operation::Cancel,
    Operation::Delete,
    Operation::Rename,
    Operation::UpdatePosted,
];

/// The effects of one planned step, then those of the step it wraps, in the order they can
/// happen.
pub(crate) fn planned_effects(
    p: &PlannedStep,
) -> impl Iterator<Item = (&StepName, &str, &EffectDecl)> {
    let own = p.effects.iter().map(|e| (&p.step, p.app.as_str(), e));
    let wrapped = p
        .wrapped
        .iter()
        .flat_map(|w| w.effects.iter().map(move |e| (&w.step, w.app.as_str(), e)));
    own.chain(wrapped)
}

/// The stores that internal effects in `plan` write.
pub(crate) fn internal_stores(plan: &[PlannedStep]) -> BTreeSet<&StoreName> {
    plan.iter()
        .flat_map(planned_effects)
        .filter(|(_, _, e)| e.kind == EffectKind::Internal)
        .flat_map(|(_, _, e)| e.writes.iter())
        .collect()
}

impl Registry {
    /// What `operation` on `doctype` declares it does to the world, in the order its steps run
    /// (the ADR-51 sequence), after every override: a `Replace` takes its replacement's
    /// declaration, a `Disable` drops the step's effects, a `Wrap` contributes the wrapper's
    /// effects and then the wrapped step's. Steps without declarations contribute nothing, so
    /// an app that declares no effects gets an empty list.
    pub fn effects(&self, doctype: &str, operation: Operation) -> Vec<DeclaredEffect> {
        let mut out = Vec::new();
        for stage in sequence(operation) {
            let Stage::Hook(point) = stage else { continue };
            for p in self.plan(doctype, &point) {
                for (step, app, decl) in planned_effects(p) {
                    out.push(DeclaredEffect {
                        step: step.clone(),
                        app: app.to_owned(),
                        point: point.clone(),
                        decl: decl.clone(),
                    });
                }
            }
        }
        out
    }

    /// The overrides that change which effects happen, for the admin override listing: each
    /// row names the override the way [`OverrideReport`](crate::OverrideReport) does (app, kind,
    /// target step, DocType, hook point) and what it removes and adds. Sorted, and independent
    /// of registration order.
    pub fn effect_changing_overrides(&self) -> &[EffectChange] {
        &self.effect_changes
    }
}

/// Checks a receipt against what the pipeline declares (ADR-080 6.4). The engine logs the result
/// in production; the conformance kit fails on any entry.
///
/// An effect is covered when a step in the pipeline of `operation` on `doctype` declares an
/// effect of the same class **and** kind. A receipt for another DocType or operation is not
/// covered at all (fail closed). An empty receipt is always valid.
pub fn check_receipt(
    registry: &Registry,
    doctype: &str,
    operation: Operation,
    receipt: &OperationReceipt,
) -> Vec<Undeclared> {
    let mismatch = receipt.doctype != doctype || receipt.operation != operation;
    let declared = if mismatch {
        Vec::new()
    } else {
        registry.effects(doctype, operation)
    };
    receipt
        .effects
        .iter()
        .filter_map(|r| {
            let reason = if mismatch {
                UndeclaredReason::ReceiptMismatch
            } else {
                let of_step: Vec<&DeclaredEffect> =
                    declared.iter().filter(|d| d.step == r.step).collect();
                if of_step
                    .iter()
                    .any(|d| d.decl.class == r.class && d.decl.kind == r.kind)
                {
                    return None;
                }
                if of_step.is_empty() {
                    UndeclaredReason::NoDeclaration
                } else if of_step.iter().any(|d| d.decl.class == r.class) {
                    UndeclaredReason::KindMismatch
                } else {
                    UndeclaredReason::ClassNotDeclared
                }
            };
            Some(Undeclared {
                step: r.step.clone(),
                class: r.class.clone(),
                kind: r.kind,
                reason,
            })
        })
        .collect()
}

/// The after-phase hook point of an operation.
pub(crate) fn after(op: Operation) -> HookPoint {
    HookPoint::of(Phase::After, Target::Op(op))
}

/// The operation pairs this crate checks: `(forward, reversing)`.
pub(crate) fn pairs() -> impl Iterator<Item = (Operation, Operation)> {
    OPERATIONS
        .into_iter()
        .filter_map(|op| reversing_operation(op).map(|rev| (op, rev)))
}
