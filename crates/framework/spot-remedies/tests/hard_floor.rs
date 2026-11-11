//! The hard floor: a submitted financial record changes only through cancel, amend or a journal
//! entry, and the context enforces it.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use serde_json::json;
use spot_agent_tiers::Policy;
use spot_remedies::seeds::RecomputeField;
use spot_remedies::{
    DocOp, DocRef, DocState, DocTypeRef, FakeWorld, Registry, RemedyError, RemedyId,
};

#[test]
fn a_direct_edit_of_a_submitted_financial_record_is_refused() {
    let settings = common::settings();
    let mut world = FakeWorld::new();
    world.document.seed("SINV-1", DocState::Submitted, true);
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let doc = DocRef::new("SINV-1");
    let error = ctx
        .apply_document(
            &doc,
            DocOp::Recompute {
                field: "total".to_owned(),
            },
        )
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::HardFloorFinancial { .. }),
        "got {error}"
    );
    // The document is untouched.
    assert_eq!(world.doc_state("SINV-1"), Some(DocState::Submitted));
}

#[test]
fn cancel_amend_and_journal_are_allowed_on_a_submitted_financial_record() {
    let settings = common::settings();
    let doc = DocRef::new("SINV-1");

    let mut world = FakeWorld::new();
    world.document.seed("SINV-1", DocState::Submitted, true);
    {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        ctx.apply_document(
            &doc,
            DocOp::Cancel {
                reason: "duplicate".to_owned(),
            },
        )
        .unwrap();
    }
    assert_eq!(world.doc_state("SINV-1"), Some(DocState::Cancelled));

    let mut world = FakeWorld::new();
    world.document.seed("SINV-1", DocState::Submitted, true);
    {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        ctx.apply_document(&doc, DocOp::Amend).unwrap();
        ctx.apply_document(
            &doc,
            DocOp::JournalEntry {
                entry: "JE-1".to_owned(),
            },
        )
        .unwrap();
    }
}

#[test]
fn a_draft_financial_record_may_be_recomputed() {
    let settings = common::settings();
    let mut world = FakeWorld::new();
    world.document.seed("SINV-1", DocState::Draft, true);
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    ctx.apply_document(
        &DocRef::new("SINV-1"),
        DocOp::Recompute {
            field: "total".to_owned(),
        },
    )
    .unwrap();
}

#[test]
fn a_remedy_that_directly_edits_a_submitted_financial_record_is_refused() {
    let settings = common::settings();
    let registry = Registry::new(
        vec![Box::new(RecomputeField {
            affects: DocTypeRef::new("Document").unwrap(),
        })],
        Policy::default(),
        settings.clone(),
    )
    .unwrap();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("document.recompute_field");
    let gate = common::gate();
    let mut world = FakeWorld::new();
    world.document.seed("SINV-1", DocState::Submitted, true);

    let plan = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry
            .dry_run(
                &agent,
                &id,
                &json!({"document": "SINV-1", "field": "total"}),
                &gate,
                &mut ctx,
            )
            .unwrap()
    };
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let error = registry
        .apply(&agent, &id, &plan, &gate, &mut ctx)
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::HardFloorFinancial { .. }),
        "got {error}"
    );
}
