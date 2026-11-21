//! The quarantine, BPM-case and document remedies: the seeds whose operations run through the
//! quarantine queue, the BPM engine and the document lifecycle. Split from [`crate::seeds`] so no
//! one file carries the whole seed catalogue; they share its helpers.

use serde_json::{Value as Json, json};
use spot_agent_tiers::Tier;

use crate::ctx::RemedyCtx;
use crate::error::RemedyError;
use crate::fixture::FakeWorld;
use crate::model::{
    Applied, Change, Check, ParamSchema, ParamType, Plan, PlanStep, RemedyId, RemedyMeta, Risk,
    UndoKind, UndoToken, Undone,
};
use crate::ports::{CaseChange, CaseLife, CaseRef, DocOp, QuarantineState};
use crate::remedy::Remedy;
use crate::scenario::{Scenario, TestSpec, UndoExpectation, WorldSpec};
use crate::seeds::{blocked, op_err, str_param, target_key};

// ---------------------------------------------------------------------------------------------
// quarantine.retry_all
// ---------------------------------------------------------------------------------------------

/// Retries every held quarantined item. Irreversible: the items leave quarantine.
pub(crate) struct QuarantineRetryAll;

impl Remedy for QuarantineRetryAll {
    fn id(&self) -> RemedyId {
        RemedyId::new(concat!("quarantine", ".retry_all"))
    }

    fn meta(&self) -> RemedyMeta {
        RemedyMeta {
            title: "Retry every held quarantined item".to_owned(),
            tier: Tier::T3,
            risk: Risk::High,
            affects: Vec::new(),
            params: ParamSchema::empty(),
            preconditions: "At least one item is held in quarantine.".to_owned(),
            undo: UndoKind::irreversible(
                "retrying releases items into the system; their journey cannot be recalled",
            ),
        }
    }

    fn precondition(&self, ctx: &mut RemedyCtx<'_>, _params: &Json) -> Check {
        let held = ctx.quarantine().held();
        if held.is_empty() {
            blocked("no item is held in quarantine")
        } else {
            Check::ready(&format!("{} item(s) are held", held.len()))
        }
    }

    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError> {
        let held = ctx.quarantine().held();
        if held.is_empty() {
            return Err(RemedyError::PreconditionFailed {
                remedy: self.id().as_str().to_owned(),
                detail: "no item is held in quarantine".to_owned(),
            });
        }
        let steps = held
            .iter()
            .map(|id| {
                PlanStep::new(
                    &format!("quarantine:{}", id.as_str()),
                    "quarantine_retry",
                    "retry the held item",
                )
            })
            .collect();
        Ok(Plan::seal(self.id(), params.clone(), steps))
    }

    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError> {
        let mut changes = Vec::new();
        for id in ctx.quarantine().held() {
            let change = ctx.quarantine().retry(&id).map_err(op_err)?;
            changes.push(Change::quarantine(&id, change.before, change.after));
        }
        Ok(Applied {
            remedy: self.id(),
            plan_hash: plan.hash.clone(),
            changes,
            undo_token: UndoToken::derive(&plan.hash, ctx.now(), 0),
        })
    }

    fn undo(&self, _ctx: &mut RemedyCtx<'_>, _applied: &Applied) -> Result<Undone, RemedyError> {
        Err(RemedyError::Irreversible {
            remedy: self.id().as_str().to_owned(),
            reason: "retrying releases items into the system; their journey cannot be recalled"
                .to_owned(),
        })
    }

    fn self_test(&self) -> TestSpec {
        TestSpec::new(Scenario::new(
            "every held item is retried; the change is irreversible",
            WorldSpec::new()
                .with_quarantine("q1", QuarantineState::Held)
                .with_quarantine("q2", QuarantineState::Held),
            json!({}),
            UndoExpectation::Refused,
            quarantine_retry_applied,
            quarantine_retry_unchanged,
        ))
    }
}

/// The invariant after retry: both items are retrying.
fn quarantine_retry_applied(world: &FakeWorld) -> Result<(), String> {
    for id in ["q1", "q2"] {
        match world.quarantine_state(id) {
            Some(QuarantineState::Retrying) => {}
            other => return Err(format!("{id} is {other:?}, not retrying")),
        }
    }
    Ok(())
}

/// The invariant after the refused undo: both items are still retrying.
fn quarantine_retry_unchanged(world: &FakeWorld) -> Result<(), String> {
    quarantine_retry_applied(world)
}

// ---------------------------------------------------------------------------------------------
// case.unstick
// ---------------------------------------------------------------------------------------------

/// Unsticks a BPM case through the engine's own operations.
pub(crate) struct CaseUnstick;

impl Remedy for CaseUnstick {
    fn id(&self) -> RemedyId {
        RemedyId::new(concat!("case", ".unstick"))
    }

    fn meta(&self) -> RemedyMeta {
        RemedyMeta {
            title: "Unstick a BPM case".to_owned(),
            tier: Tier::T2,
            risk: Risk::Medium,
            affects: Vec::new(),
            params: ParamSchema::empty().with_required("case_id", ParamType::String),
            preconditions: "The case exists and is stuck.".to_owned(),
            undo: UndoKind::reversible("the case is set back to stuck"),
        }
    }

    fn precondition(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Check {
        let Ok(case_id) = str_param(&self.id(), params, "case_id") else {
            return blocked("the case_id parameter is missing");
        };
        let case = CaseRef::new(&case_id);
        match ctx.case().case(&case) {
            Some(view) if view.life == CaseLife::Stuck => {
                Check::ready(&format!("case `{}` is stuck", case.as_str()))
            }
            Some(view) => blocked(&format!(
                "case `{}` is {}, not stuck",
                case.as_str(),
                view.life.as_str()
            )),
            None => blocked(&format!("no case `{}` exists", case.as_str())),
        }
    }

    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError> {
        let case = CaseRef::new(&str_param(&self.id(), params, "case_id")?);
        let view = ctx
            .case()
            .case(&case)
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "case_unstick".to_owned(),
                detail: format!("no case `{}` exists", case.as_str()),
            })?;
        if view.life != CaseLife::Stuck {
            return Err(RemedyError::PreconditionFailed {
                remedy: self.id().as_str().to_owned(),
                detail: format!("case `{}` is not stuck", case.as_str()),
            });
        }
        let step = PlanStep::new(
            &format!("case:{}", case.as_str()),
            "case_unstick",
            "unstick the case through the BPM engine",
        );
        Ok(Plan::seal(self.id(), params.clone(), vec![step]))
    }

    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError> {
        let case = CaseRef::new(&str_param(&self.id(), &plan.params, "case_id")?);
        let now = ctx.now();
        let change: CaseChange = ctx.case().unstick(&case, now).map_err(op_err)?;
        Ok(Applied {
            remedy: self.id(),
            plan_hash: plan.hash.clone(),
            changes: vec![Change::case(&case, change.before, change.after)],
            undo_token: UndoToken::derive(&plan.hash, now, 0),
        })
    }

    fn undo(&self, ctx: &mut RemedyCtx<'_>, applied: &Applied) -> Result<Undone, RemedyError> {
        let change = applied
            .changes
            .first()
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "case_restore".to_owned(),
                detail: "the applied change is empty".to_owned(),
            })?;
        let key = target_key(&change.target, "case").ok_or_else(|| RemedyError::OpFailed {
            operation: "case_restore".to_owned(),
            detail: format!("`{}` is not a case target", change.target),
        })?;
        let life = CaseLife::parse(&change.before).ok_or_else(|| RemedyError::OpFailed {
            operation: "case_restore".to_owned(),
            detail: format!("`{}` is not a case state", change.before),
        })?;
        let case_change = CaseChange {
            id: CaseRef::new(key),
            before: life,
            after: CaseLife::Running,
        };
        ctx.case().restore(&case_change).map_err(op_err)?;
        Ok(Undone {
            remedy: self.id(),
            token: applied.undo_token.clone(),
            changes: applied.changes.clone(),
        })
    }

    fn self_test(&self) -> TestSpec {
        TestSpec::new(Scenario::new(
            "a stuck case is unstuck and then restored",
            WorldSpec::new().with_case("case1", CaseLife::Stuck),
            json!({"case_id": "case1"}),
            UndoExpectation::Restored,
            case_unstick_applied,
            case_unstick_undone,
        ))
    }
}

/// The invariant after unsticking: the case is running.
fn case_unstick_applied(world: &FakeWorld) -> Result<(), String> {
    match world.case_life("case1") {
        Some(CaseLife::Running) => Ok(()),
        other => Err(format!("case1 is {other:?}, not running")),
    }
}

/// The invariant after undoing: the case is stuck again.
fn case_unstick_undone(world: &FakeWorld) -> Result<(), String> {
    match world.case_life("case1") {
        Some(CaseLife::Stuck) => Ok(()),
        other => Err(format!("case1 is {other:?}, not stuck")),
    }
}

/// A document remedy used by the hard-floor tests: it recomputes a derived field, which the
/// context refuses for a submitted financial record.
///
/// It is not a seed (the seed list is fixed), so it lives here for the tests to register and
/// prove the context's enforcement.
pub struct RecomputeField {
    /// The document type it affects.
    pub affects: crate::model::DocTypeRef,
}

impl Remedy for RecomputeField {
    fn id(&self) -> RemedyId {
        RemedyId::new(concat!("document", ".recompute_field"))
    }

    fn meta(&self) -> RemedyMeta {
        RemedyMeta {
            title: "Recompute a derived field".to_owned(),
            tier: Tier::T2,
            risk: Risk::Medium,
            affects: vec![self.affects.clone()],
            params: ParamSchema::empty()
                .with_required("document", ParamType::String)
                .with_required("field", ParamType::String),
            preconditions: "The document exists.".to_owned(),
            undo: UndoKind::reversible("the field is recomputed again from its source"),
        }
    }

    fn precondition(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Check {
        let Ok(doc) = str_param(&self.id(), params, "document") else {
            return blocked("the document parameter is missing");
        };
        if ctx
            .document()
            .view(&crate::ports::DocRef::new(&doc))
            .is_some()
        {
            Check::ready(&format!("document `{doc}` exists"))
        } else {
            blocked(&format!("no document `{doc}` exists"))
        }
    }

    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError> {
        let doc = crate::ports::DocRef::new(&str_param(&self.id(), params, "document")?);
        let field = str_param(&self.id(), params, "field")?;
        if ctx.document().view(&doc).is_none() {
            return Err(RemedyError::PreconditionFailed {
                remedy: self.id().as_str().to_owned(),
                detail: format!("no document `{}` exists", doc.as_str()),
            });
        }
        let step = PlanStep::new(
            &format!("doc:{}", doc.as_str()),
            "document_recompute",
            &format!("recompute `{field}`"),
        );
        Ok(Plan::seal(self.id(), params.clone(), vec![step]))
    }

    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError> {
        let doc = crate::ports::DocRef::new(&str_param(&self.id(), &plan.params, "document")?);
        let field = str_param(&self.id(), &plan.params, "field")?;
        let before = ctx
            .document()
            .view(&doc)
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "document_recompute".to_owned(),
                detail: format!("no document `{}` exists", doc.as_str()),
            })?;
        let change = ctx.apply_document(&doc, DocOp::Recompute { field })?;
        Ok(Applied {
            remedy: self.id(),
            plan_hash: plan.hash.clone(),
            changes: vec![Change::document(&doc, before, change.after)],
            undo_token: UndoToken::derive(&plan.hash, ctx.now(), 0),
        })
    }

    fn undo(&self, ctx: &mut RemedyCtx<'_>, applied: &Applied) -> Result<Undone, RemedyError> {
        // Recomputing is idempotent; the reversal records the change without a further write.
        let _ = ctx;
        Ok(Undone {
            remedy: self.id(),
            token: applied.undo_token.clone(),
            changes: applied.changes.clone(),
        })
    }

    fn self_test(&self) -> TestSpec {
        TestSpec::new(Scenario::new(
            "a derived field is recomputed on a draft document",
            WorldSpec::new().with_document("SINV-1", crate::ports::DocState::Draft, false),
            json!({"document": "SINV-1", "field": "total"}),
            UndoExpectation::Restored,
            recompute_applied,
            recompute_undone,
        ))
    }
}

/// The invariant after recomputing a draft: it is still a draft.
fn recompute_applied(world: &FakeWorld) -> Result<(), String> {
    match world.doc_state("SINV-1") {
        Some(crate::ports::DocState::Draft) => Ok(()),
        other => Err(format!("SINV-1 is {other:?}, not draft")),
    }
}

/// The invariant after the reversal: it is still a draft.
fn recompute_undone(world: &FakeWorld) -> Result<(), String> {
    recompute_applied(world)
}
