//! Tier gating, the unknown remedy, parameter validation, the hard floor and the mandatory
//! dry-run.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use serde_json::json;
use spot_agent_tiers::Tier;
use spot_remedies::{FakeWorld, Gate, Plan, Registry, RemedyError, RemedyId};

fn registry() -> Registry {
    Registry::seeds().unwrap()
}

fn job_world() -> FakeWorld {
    let mut world = FakeWorld::new();
    world
        .jobs
        .seed("j1", spot_remedies::JobLife::Failed, 3, None);
    world
}

#[test]
fn a_remedy_above_the_agents_tier_is_refused() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::support_agent("agent-t1", Tier::T1);
    let mut world = job_world();
    let mut ctx = world.ctx("agent-t1", common::NOW, &settings);
    // index.rebuild_retrieval is T2; the agent tops out at T1.
    let id = RemedyId::new("index.rebuild_retrieval");
    let error = registry
        .dry_run(
            &agent,
            &id,
            &json!({"index": "retrieval"}),
            &common::gate(),
            &mut ctx,
        )
        .unwrap_err();
    match error {
        RemedyError::Refused { code, .. } => assert_eq!(code, "agent.above_agent_max_tier"),
        other => panic!("expected a tier refusal, got {other}"),
    }
}

#[test]
fn an_operations_agent_is_refused_a_support_remedy() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::operations_agent("agent-ops", Tier::T2);
    let mut world = job_world();
    let mut ctx = world.ctx("agent-ops", common::NOW, &settings);
    let id = RemedyId::new("job.retry_failed");
    let error = registry
        .dry_run(
            &agent,
            &id,
            &json!({"job_id": "j1"}),
            &common::gate(),
            &mut ctx,
        )
        .unwrap_err();
    match error {
        RemedyError::Refused { code, .. } => assert_eq!(code, "agent.domain_mismatch"),
        other => panic!("expected a domain refusal, got {other}"),
    }
}

#[test]
fn an_unknown_remedy_is_refused() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let mut world = job_world();
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let id = RemedyId::new("nope.missing");
    let error = registry
        .dry_run(&agent, &id, &json!({}), &common::gate(), &mut ctx)
        .unwrap_err();
    assert!(matches!(error, RemedyError::Unknown { .. }), "got {error}");
}

#[test]
fn invalid_parameters_are_refused() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let mut world = job_world();
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let id = RemedyId::new("job.retry_failed");
    // job_id is required and missing.
    let error = registry
        .dry_run(&agent, &id, &json!({}), &common::gate(), &mut ctx)
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::ParamsInvalid { .. }),
        "got {error}"
    );
}

#[test]
fn a_t3_remedy_needs_approval_for_a_non_deep_agent() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::support_agent("agent-t3", Tier::T3);
    let mut world = FakeWorld::new();
    world
        .quarantine
        .seed("q1", spot_remedies::QuarantineState::Held);
    let mut ctx = world.ctx("agent-t3", common::NOW, &settings);
    let id = RemedyId::new("quarantine.retry_all");
    let error = registry
        .dry_run(&agent, &id, &json!({}), &common::gate(), &mut ctx)
        .unwrap_err();
    match error {
        RemedyError::Refused { code, .. } => assert_eq!(code, "agent.tier_needs_approval"),
        other => panic!("expected an approval refusal, got {other}"),
    }
}

#[test]
fn the_hard_floor_refuses_a_change_without_the_journal() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let mut world = job_world();
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let id = RemedyId::new("job.retry_failed");
    let mut gate = Gate::change();
    gate.journaled = false;
    let error = registry
        .dry_run(&agent, &id, &json!({"job_id": "j1"}), &gate, &mut ctx)
        .unwrap_err();
    match error {
        RemedyError::Refused { code, .. } => assert_eq!(code, "agent.hard_floor_not_journaled"),
        other => panic!("expected a journal refusal, got {other}"),
    }
}

#[test]
fn apply_refuses_a_forged_plan() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    let gate = common::gate();

    // A genuine plan.
    let plan = {
        let mut world = job_world();
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry
            .dry_run(&agent, &id, &json!({"job_id": "j1"}), &gate, &mut ctx)
            .unwrap()
    };

    // Tampered parameters keep the old hash: the plan no longer binds them.
    let mut forged = plan.clone();
    forged.params = json!({"job_id": "j2"});
    let mut world = job_world();
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let error = registry
        .apply(&agent, &id, &forged, &gate, &mut ctx)
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::PlanStale { .. }),
        "got {error}"
    );
}

#[test]
fn apply_refuses_a_plan_from_another_remedy() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    let other = RemedyId::new("cache.flush_namespace");
    let plan = Plan::seal(other, json!({"job_id": "j1"}), Vec::new());
    let mut world = job_world();
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let error = registry
        .apply(&agent, &id, &plan, &common::gate(), &mut ctx)
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::PlanStale { .. }),
        "got {error}"
    );
}

#[test]
fn a_refusal_is_audited() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::support_agent("agent-t1", Tier::T1);
    let mut world = job_world();
    let mut ctx = world.ctx("agent-t1", common::NOW, &settings);
    let id = RemedyId::new("index.rebuild_retrieval");
    let _ = registry.dry_run(
        &agent,
        &id,
        &json!({"index": "retrieval"}),
        &common::gate(),
        &mut ctx,
    );
    assert_eq!(world.audit.count(spot_remedies::AuditKind::Refused), 1);
    assert_eq!(world.audit.count(spot_remedies::AuditKind::Authorised), 0);
}

#[test]
fn apply_refuses_a_plan_the_registry_never_issued() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    // A caller seals its own plan: the hash is valid, but no dry-run issued it.
    let plan = Plan::seal(id.clone(), json!({"job_id": "j1"}), Vec::new());
    let mut world = job_world();
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let error = registry
        .apply(&agent, &id, &plan, &common::gate(), &mut ctx)
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::PlanStale { .. }),
        "got {error}"
    );
}

#[test]
fn a_plan_applies_once() {
    let registry = registry();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    let gate = common::gate();
    let mut world = job_world();
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let plan = registry
        .dry_run(&agent, &id, &json!({"job_id": "j1"}), &gate, &mut ctx)
        .unwrap();
    registry.apply(&agent, &id, &plan, &gate, &mut ctx).unwrap();
    let replay = registry
        .apply(&agent, &id, &plan, &gate, &mut ctx)
        .unwrap_err();
    assert!(
        matches!(replay, RemedyError::PlanStale { .. }),
        "got {replay}"
    );
}
