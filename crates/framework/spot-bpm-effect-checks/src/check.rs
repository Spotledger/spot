//! The design-time checks (ADR-080 6.8).

use spot_app_api::{EffectKind, Operation, Reversal};
use spot_bpm_model::ProcessVersion;
use spot_seams::{DeclaredEffect, Registry};

use crate::declared::{Source, node_effects, node_operation};
use crate::graph::Shape;
use crate::meta::{EdgeMode, EffectMeta};
use crate::problem::{Problem, ProblemKind, Undo};

/// Checks a process version against the effect declarations in `registry`.
///
/// Pure and deterministic: it never runs the process, reads no clock and returns the problems in
/// one stable order (by kind, then node, edge and class), whatever the order of nodes and edges.
/// A process beyond the bounds of [`crate::MAX_NODES`], [`crate::MAX_EDGES`] or
/// [`crate::MAX_BACK_EDGES`] edges, or more than [`crate::MAX_COMMIT_POINTS`] commit points, yields one `too_large` error and nothing else (fail closed).
///
/// - `undo_past_irreversible` (error): a backward edge the meta marks undo, whose range holds a
///   document node with an `Irreversible` effect (it has no compensation: `CompensableBy` is a
///   different declaration).
/// - `no_declared_reversal` (error): a document node whose operation declares an internal effect
///   that is not `ReversibleBy`, or is `ReversibleBy` an operation whose own pipeline declares no
///   internal effect to reverse it.
/// - `no_declared_pipeline` (warning): a document node whose record type and operation the
///   registry plans no step for at all. Its effects are unknown, not empty, so an undo across it
///   is unchecked.
/// - `empty_commit_point` (warning): a commit point whose region declares no external effect that
///   can be held.
/// - `rewind_would_duplicate` (error): a backward edge marked rewind, whose range holds a node
///   that creates a record and has no revision mapping.
/// - `unknown_effect_class` (error): a document node declares a class the site does not know.
/// - `held_region_contains_never_held` (warning): a never-held external effect inside a held
///   region.
#[must_use]
pub fn check(process: &ProcessVersion, registry: &Registry, meta: &dyn EffectMeta) -> Vec<Problem> {
    check_with(process, registry, meta)
}

pub(crate) fn check_with(
    process: &ProcessVersion,
    source: &dyn Source,
    meta: &dyn EffectMeta,
) -> Vec<Problem> {
    let Some(shape) = Shape::of(process, meta) else {
        return vec![Problem::new(ProblemKind::TooLarge)];
    };
    // The declared effects of every node, read from the registry once.
    let effects: Vec<Vec<DeclaredEffect>> = shape
        .nodes
        .iter()
        .map(|node| node_effects(source, node))
        .collect();
    let mut problems = Vec::new();
    node_checks(&shape, &effects, source, meta, &mut problems);
    back_edge_checks(&shape, &effects, meta, &mut problems);
    commit_checks(&shape, &effects, meta, &mut problems);
    problems.sort();
    problems.dedup();
    problems
}

fn node_checks(
    shape: &Shape<'_>,
    effects: &[Vec<DeclaredEffect>],
    source: &dyn Source,
    meta: &dyn EffectMeta,
    out: &mut Vec<Problem>,
) {
    for (node, declared) in shape.nodes.iter().zip(effects) {
        for d in declared {
            if !meta.is_known_class(&d.decl.class) {
                out.push(
                    Problem::new(ProblemKind::UnknownEffectClass)
                        .at(&node.id)
                        .of_class(d.decl.class.as_str()),
                );
            }
        }
        let Some((doctype, op)) = node_operation(node) else {
            continue;
        };
        if declared.is_empty() && !source.has_pipeline(doctype, op) {
            out.push(Problem::new(ProblemKind::NoDeclaredPipeline).at(&node.id));
        }
        for d in declared
            .iter()
            .filter(|d| d.decl.kind == EffectKind::Internal)
        {
            let reversed = match &d.decl.reversal {
                Reversal::ReversibleBy(op) => reversal_declared(source, doctype, *op),
                _ => false,
            };
            if !reversed {
                out.push(
                    Problem::new(ProblemKind::NoDeclaredReversal)
                        .at(&node.id)
                        .of_class(d.decl.class.as_str()),
                );
            }
        }
    }
}

/// Whether the pipeline of `op` on `doctype` declares an internal effect: something that does the
/// reversing.
fn reversal_declared(source: &dyn Source, doctype: &str, op: Operation) -> bool {
    source
        .effects(doctype, op)
        .iter()
        .any(|d| d.decl.kind == EffectKind::Internal)
}

/// Whether the effect cannot be undone. Anything that is not reversible or compensable counts,
/// including a reversal a later contract adds (the same fail-closed reading as the summary).
fn is_irreversible(d: &DeclaredEffect) -> bool {
    Undo::from(d.decl.reversal.reversibility()) == Undo::Irreversible
}

fn back_edge_checks(
    shape: &Shape<'_>,
    effects: &[Vec<DeclaredEffect>],
    meta: &dyn EffectMeta,
    out: &mut Vec<Problem>,
) {
    for back in shape.back_edges() {
        let (from, to) = (&back.edge.from, &back.edge.to);
        for &i in &back.range {
            let node = &shape.nodes[i];
            match back.mode {
                EdgeMode::Undo => {
                    for d in effects[i].iter().filter(|d| is_irreversible(d)) {
                        out.push(
                            Problem::new(ProblemKind::UndoPastIrreversible)
                                .at(&node.id)
                                .on_edge(from, to)
                                .of_class(d.decl.class.as_str()),
                        );
                    }
                }
                EdgeMode::Rewind => {
                    // The same operation test as the declarations use.
                    let creates =
                        node_operation(node).is_some_and(|(_, op)| op == Operation::Create);
                    if creates && !meta.has_revision(&node.id) {
                        out.push(
                            Problem::new(ProblemKind::RewindWouldDuplicate)
                                .at(&node.id)
                                .on_edge(from, to),
                        );
                    }
                }
                EdgeMode::Forward => {}
            }
        }
    }
}

fn commit_checks(
    shape: &Shape<'_>,
    effects: &[Vec<DeclaredEffect>],
    meta: &dyn EffectMeta,
    out: &mut Vec<Problem>,
) {
    // Per node: leaves at once (never held) or can be held.
    let never_held: Vec<bool> = effects
        .iter()
        .map(|e| {
            e.iter()
                .any(|d| d.decl.kind == EffectKind::External && meta.is_never_held(&d.decl.class))
        })
        .collect();
    let holdable: Vec<bool> = effects
        .iter()
        .map(|e| {
            e.iter()
                .any(|d| d.decl.kind == EffectKind::External && !meta.is_never_held(&d.decl.class))
        })
        .collect();
    let mut held = vec![false; shape.nodes.len()];
    for &commit in &shape.commits {
        let region = shape.region(commit);
        if !region.iter().zip(&holdable).any(|(r, h)| *r && *h) {
            out.push(Problem::new(ProblemKind::EmptyCommitPoint).at(&shape.nodes[commit].id));
        }
        for (h, r) in held.iter_mut().zip(&region) {
            *h |= *r;
        }
    }
    for (i, node) in shape.nodes.iter().enumerate() {
        if !(held[i] && never_held[i]) {
            continue;
        }
        for d in &effects[i] {
            if d.decl.kind == EffectKind::External && meta.is_never_held(&d.decl.class) {
                out.push(
                    Problem::new(ProblemKind::HeldRegionContainsNeverHeld)
                        .at(&node.id)
                        .of_class(d.decl.class.as_str()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::meta::PolicyMeta;
    use spot_app_api::{
        EffectClassName, EffectDecl, HookPoint, Phase, StepName, StoreName, Target,
    };
    use spot_bpm_model::{DocumentAction, DocumentConfig, Node, NodeId, NodeKind, VersionStatus};

    /// A table source, for shapes a built registry rejects.
    struct Table(Vec<(String, Operation, DeclaredEffect)>);

    impl Source for Table {
        fn effects(&self, doctype: &str, operation: Operation) -> Vec<DeclaredEffect> {
            self.0
                .iter()
                .filter(|(d, o, _)| d == doctype && *o == operation)
                .map(|(_, _, e)| e.clone())
                .collect()
        }

        fn has_pipeline(&self, doctype: &str, operation: Operation) -> bool {
            self.0
                .iter()
                .any(|(d, o, _)| d == doctype && *o == operation)
        }
    }

    fn declared(kind: EffectKind, reversal: Reversal) -> DeclaredEffect {
        DeclaredEffect {
            step: StepName::new("ledger:write").unwrap(),
            app: "ledger".to_owned(),
            point: HookPoint::of(Phase::After, Target::Op(Operation::Create)),
            decl: EffectDecl::new(
                kind,
                EffectClassName::new("balance_movement").unwrap(),
                vec![StoreName::new("ledger_entry").unwrap()],
                reversal,
            ),
        }
    }

    fn one_node_process() -> ProcessVersion {
        let config = DocumentConfig {
            action: DocumentAction::Create,
            doctype: "entry".to_owned(),
            operation: None,
            mapping: None,
            principal: None,
        };
        ProcessVersion::new(
            "p",
            "app",
            1,
            VersionStatus::Draft,
            vec![Node::new(
                NodeId::new("make").unwrap(),
                NodeKind::Document(config),
            )],
            Vec::new(),
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn an_internal_effect_that_is_not_reversible_is_a_problem() {
        let table = Table(vec![(
            "entry".to_owned(),
            Operation::Create,
            declared(
                EffectKind::Internal,
                Reversal::Irreversible {
                    reason: "x".to_owned(),
                },
            ),
        )]);
        let problems = check_with(&one_node_process(), &table, &PolicyMeta::new());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].token(), "no_declared_reversal");
    }

    #[test]
    fn an_internal_effect_with_a_reversing_pipeline_is_clean() {
        let table = Table(vec![
            (
                "entry".to_owned(),
                Operation::Create,
                declared(
                    EffectKind::Internal,
                    Reversal::ReversibleBy(Operation::Delete),
                ),
            ),
            (
                "entry".to_owned(),
                Operation::Delete,
                declared(
                    EffectKind::Internal,
                    Reversal::ReversibleBy(Operation::Create),
                ),
            ),
        ]);
        assert!(check_with(&one_node_process(), &table, &PolicyMeta::new()).is_empty());
    }
}
