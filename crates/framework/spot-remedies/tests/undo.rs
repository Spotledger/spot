//! Undo: the conflict check, the expiry window, the irreversible refusal and the audit trail.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use serde_json::json;
use spot_remedies::{
    AuditKind, FakeWorld, JobLife, JobOps, JobRef, QuarantineState, Registry, RemedyError,
    RemedyId, RemedySettings,
};

fn job_world() -> FakeWorld {
    let mut world = FakeWorld::new();
    world.jobs.seed("j1", JobLife::Failed, 3, None);
    world
}

#[test]
fn undo_refuses_when_a_later_change_conflicts() {
    let registry = Registry::seeds().unwrap();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    let gate = common::gate();
    let mut world = job_world();

    let plan = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry
            .dry_run(&agent, &id, &json!({"job_id": "j1"}), &gate, &mut ctx)
            .unwrap()
    };
    let applied = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry.apply(&agent, &id, &plan, &gate, &mut ctx).unwrap()
    };

    // A later change moves the job away from the state the apply left.
    world
        .jobs
        .restore(&JobRef::new("j1"), JobLife::Running)
        .unwrap();

    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let error = registry
        .undo(&agent, &id, &applied.undo_token, &gate, &mut ctx)
        .unwrap_err();
    match error {
        RemedyError::UndoConflict { target, .. } => assert_eq!(target, "job:j1"),
        other => panic!("expected an undo conflict, got {other}"),
    }
    assert_eq!(world.audit.count(AuditKind::UndoRefused), 1);
}

#[test]
fn undo_refuses_after_the_window_closes() {
    let settings = RemedySettings {
        undo_window_secs: 10,
        ..RemedySettings::default()
    };
    let registry = Registry::with_seeds(settings.clone()).unwrap();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    let gate = common::gate();
    let mut world = job_world();

    let plan = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry
            .dry_run(&agent, &id, &json!({"job_id": "j1"}), &gate, &mut ctx)
            .unwrap()
    };
    let applied = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry.apply(&agent, &id, &plan, &gate, &mut ctx).unwrap()
    };

    let mut ctx = world.ctx("agent-deep", common::NOW + 11, &settings);
    let error = registry
        .undo(&agent, &id, &applied.undo_token, &gate, &mut ctx)
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::UndoExpired { .. }),
        "got {error}"
    );
}

#[test]
fn undo_refuses_an_irreversible_remedy() {
    let registry = Registry::seeds().unwrap();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("quarantine.retry_all");
    let gate = common::gate();
    let mut world = FakeWorld::new();
    world.quarantine.seed("q1", QuarantineState::Held);

    let plan = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry
            .dry_run(&agent, &id, &json!({}), &gate, &mut ctx)
            .unwrap()
    };
    let applied = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry.apply(&agent, &id, &plan, &gate, &mut ctx).unwrap()
    };

    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let error = registry
        .undo(&agent, &id, &applied.undo_token, &gate, &mut ctx)
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::Irreversible { .. }),
        "got {error}"
    );
    assert_eq!(world.audit.count(AuditKind::UndoRefused), 1);
}

#[test]
fn a_successful_undo_is_audited_and_restores_the_state() {
    let registry = Registry::seeds().unwrap();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    let gate = common::gate();
    let mut world = job_world();

    let plan = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry
            .dry_run(&agent, &id, &json!({"job_id": "j1"}), &gate, &mut ctx)
            .unwrap()
    };
    let applied = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry.apply(&agent, &id, &plan, &gate, &mut ctx).unwrap()
    };
    assert_eq!(world.job_life("j1"), Some(JobLife::Queued));

    {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry
            .undo(&agent, &id, &applied.undo_token, &gate, &mut ctx)
            .unwrap();
    }
    assert_eq!(world.job_life("j1"), Some(JobLife::Failed));
    assert_eq!(world.audit.count(AuditKind::Undone), 1);
    assert!(
        registry.undo_record(&applied.undo_token).is_none(),
        "the record is consumed"
    );
}

#[test]
fn undo_of_an_unknown_token_is_refused() {
    let registry = Registry::seeds().unwrap();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    let mut world = job_world();
    let token = spot_remedies::UndoToken::derive(
        &spot_remedies::PlanHash::compute(&id, &json!({}), &[]),
        common::NOW,
        999,
    );
    let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
    let error = registry
        .undo(&agent, &id, &token, &common::gate(), &mut ctx)
        .unwrap_err();
    assert!(
        matches!(error, RemedyError::UndoUnknown { .. }),
        "got {error}"
    );
}
