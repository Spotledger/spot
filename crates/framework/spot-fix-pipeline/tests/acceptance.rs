//! Acceptance tests for the fix pipeline: the state machine, the journal order, the hard floor,
//! the residue guarantee, the audit trail, the undo window and the plan normalisation.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::{FakeOptions, Harness, fix_id, labels};
use spot_agent_tiers::Tier;
use spot_audit_chain::PayloadValue;
use spot_fix_pipeline::{
    FixError, FixPlan, FixPolicy, FixSource, FixState, FixStep, Mechanism, NoFaults, Risk,
    StructuralKind,
};
use spot_remedies::{Plan as RemedyPlan, PlanStep, RemedyId, RemedyMeta, UndoKind};

#[test]
fn a_small_fix_verifies_and_opens_the_undo_window() {
    let mut h = Harness::small();
    let fix = fix_id();
    let outcome = h.run(&fix, &NoFaults).unwrap();

    assert_eq!(outcome.state, FixState::Verified);
    assert_eq!(h.world.snapshot().unwrap(), h.post_state());
    assert!(outcome.undo.unwrap().is_open(1_000));
    assert_eq!(outcome.plan_hash.as_deref(), Some(h.plan.hash.as_str()));

    let labels = labels(&h.records(&fix));
    for expected in [
        "proposed",
        "planned",
        "apply_started",
        "pre_images_captured",
        "pre_fingerprint_captured",
        "step_started",
        "step_applied",
        "applied",
        "verify_started",
        "verified",
        "undo_window_opened",
    ] {
        assert!(labels.contains(&expected), "the journal lacks `{expected}`");
    }
    assert!(
        !labels.contains(&"rehearsal_started"),
        "a rehearsal-optional low-risk T1 fix applies directly"
    );
}

#[test]
fn the_journal_is_write_ahead_of_every_step() {
    let mut h = Harness::small();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();

    let records = h.records(&fix);
    let labels = labels(&records);
    let apply = labels
        .iter()
        .position(|label| *label == "apply_started")
        .unwrap();
    let pre_images = labels
        .iter()
        .position(|label| *label == "pre_images_captured")
        .unwrap();
    let pre_fingerprint = labels
        .iter()
        .position(|label| *label == "pre_fingerprint_captured")
        .unwrap();
    let first_step = labels
        .iter()
        .position(|label| *label == "step_started")
        .unwrap();
    assert!(apply < pre_images);
    assert!(pre_images < pre_fingerprint);
    assert!(
        pre_fingerprint < first_step,
        "the pre-state is captured before any step runs"
    );

    for (index, record) in records.iter().enumerate() {
        if let spot_fix_pipeline::JournalRecord::StepApplied { change } = record {
            let started = records[..index].iter().any(|earlier| {
                matches!(
                    earlier,
                    spot_fix_pipeline::JournalRecord::StepStarted { index: started, .. }
                        if *started == change.step
                )
            });
            assert!(
                started,
                "step {} was applied before it was started",
                change.step
            );
        }
    }
}

#[test]
fn failed_verification_rolls_back_with_no_residue() {
    let mut h = Harness::with_options(
        &[("job:1", 10), ("job:2", 20)],
        Tier::T1,
        FixPolicy::default().with_rehearsal_optional(common::REMEDY),
        FakeOptions::symptom_fails(),
    );
    let fix = fix_id();
    let outcome = h.run(&fix, &NoFaults).unwrap();

    assert_eq!(outcome.state, FixState::RolledBack);
    assert_eq!(h.world.snapshot().unwrap(), h.pre_state());
    let residue = outcome.residue.expect("the rollback measured a residue");
    assert!(
        residue.is_empty(),
        "the pre-state is byte-identical by fingerprint"
    );

    let progress = h.progress(&fix);
    assert_eq!(
        progress.pre_fingerprint.as_ref().unwrap().digest(),
        h.world.fingerprint().unwrap().digest()
    );
    let labels = labels(&h.records(&fix));
    assert!(labels.contains(&"verify_failed"));
    assert!(labels.contains(&"rollback_started"));
    assert!(labels.contains(&"residue_checked"));
    assert!(labels.contains(&"rolled_back"));
    assert!(!labels.contains(&"verified"));
}

#[test]
fn a_t3_fix_rehearses_snapshots_and_asks_a_human() {
    let mut h = Harness::structural();
    let fix = fix_id();
    let outcome = h.run(&fix, &NoFaults).unwrap();

    assert_eq!(outcome.state, FixState::Verified);
    assert_eq!(h.progress(&fix).mechanism, Some(Mechanism::Snapshot));
    assert_eq!(h.approval.requests(), 1);
    assert!(h.snaps.has_snapshot());
    assert!(
        !h.snaps.gate_closed(),
        "the gate opens after a verified apply"
    );

    let labels = labels(&h.records(&fix));
    for expected in [
        "rehearsal_started",
        "rehearsed",
        "approval_requested",
        "approved",
        "snapshot_taken",
        "pre_images_captured",
        "applied",
        "verified",
    ] {
        assert!(labels.contains(&expected), "the journal lacks `{expected}`");
    }
}

#[test]
fn a_refused_t3_approval_ends_failed_without_touching_the_world() {
    let options = FakeOptions {
        approval: spot_fix_pipeline::testing::FakeApproval::refusing(),
        ..FakeOptions::default()
    };
    let mut h = Harness::with_options(
        &[("meta:party", 1), ("meta:item", 2)],
        Tier::T3,
        FixPolicy::default(),
        options,
    );
    let fix = fix_id();
    let outcome = h.run(&fix, &NoFaults).unwrap();

    assert_eq!(outcome.state, FixState::Failed);
    assert_eq!(
        outcome.failure.unwrap().stage,
        spot_fix_pipeline::FailureStage::Approval
    );
    assert_eq!(h.world.snapshot().unwrap(), h.pre_state());
    let labels = labels(&h.records(&fix));
    assert!(!labels.contains(&"apply_started"));
    assert!(!labels.contains(&"snapshot_taken"));
}

#[test]
fn a_failed_rehearsal_ends_failed_with_evidence_and_no_apply() {
    let mut h = Harness::with_options(
        &[("job:1", 10), ("job:2", 20)],
        Tier::T2,
        FixPolicy::default(),
        FakeOptions::rehearsal_fails(),
    );
    let fix = fix_id();
    let outcome = h.run(&fix, &NoFaults).unwrap();

    assert_eq!(outcome.state, FixState::Failed);
    assert_eq!(
        outcome.failure.unwrap().stage,
        spot_fix_pipeline::FailureStage::Rehearsal
    );
    assert_eq!(h.world.snapshot().unwrap(), h.pre_state());
    let labels = labels(&h.records(&fix));
    assert!(labels.contains(&"rehearsal_failed"));
    assert!(!labels.contains(&"apply_started"));
}

#[test]
fn a_verified_fix_can_be_undone_and_the_pre_state_returns() {
    let mut h = Harness::small();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();

    let outcome = h.undo(&fix, 1_010).unwrap();
    assert_eq!(outcome.state, FixState::RolledBack);
    assert_eq!(h.world.snapshot().unwrap(), h.pre_state());
    assert!(outcome.residue.unwrap().is_empty());
    let labels = labels(&h.records(&fix));
    assert!(labels.contains(&"undo_started"));
    assert!(labels.contains(&"undone"));
}

#[test]
fn undo_after_the_window_closes_refuses_and_journals_the_expiry() {
    let mut h = Harness::small();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();

    let error = h.undo(&fix, 1_000 + 86_400 + 1).unwrap_err();
    assert!(matches!(error, FixError::UndoExpired { .. }));
    let labels = labels(&h.records(&fix));
    assert!(labels.contains(&"undo_window_closed"));
    assert!(labels.contains(&"undo_refused"));
}

#[test]
fn undo_refuses_when_a_target_changed_after_the_fix() {
    let mut h = Harness::small();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();
    h.world.set("job:1", 99).unwrap();

    let error = h.undo(&fix, 1_010).unwrap_err();
    assert!(matches!(error, FixError::UndoConflict { .. }));
    let labels = labels(&h.records(&fix));
    assert!(labels.contains(&"undo_refused"));
    assert!(!labels.contains(&"undone"));
}

#[test]
fn a_snapshot_undo_detects_a_changed_site() {
    let mut h = Harness::structural();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();
    h.world.set("meta:party", 42).unwrap();

    let error = h.undo(&fix, 1_010).unwrap_err();
    assert!(matches!(error, FixError::UndoConflict { .. }));
}

#[test]
fn every_transition_is_a_valid_audit_record_with_identity_tier_and_epoch() {
    let mut h = Harness::small();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();

    let records = h.audit.records().unwrap();
    assert!(!records.is_empty());
    let events = h.audit.events_for(&fix);
    let first = events.first().unwrap();
    assert_eq!(first.agent.id, "agent-support");
    assert_eq!(first.agent.tier, Tier::T1);
    assert_eq!(first.epoch.value(), 7);

    for record in &records {
        record.validate().unwrap();
        assert!(record.kind.starts_with("fix."));
        assert_eq!(record.actor, "agent:agent-support");
        assert!(record.masked);
        assert_eq!(record.payload.get("epoch"), Some(&PayloadValue::Int(7)));
        assert_eq!(
            record.payload.get("tier"),
            Some(&PayloadValue::Text("T1".to_owned()))
        );
    }
}

#[test]
fn a_forged_plan_is_refused_before_the_journal_is_touched() {
    let mut h = Harness::small();
    let fix = fix_id();
    let mut forged = h.plan.clone();
    forged.steps[0].target = "job:999".to_owned();
    assert!(!forged.verify());

    let plan = forged.clone();
    let ctx = h.ctx.clone();
    let policy = h.policy.clone();
    let ports = spot_fix_pipeline::FixPorts {
        pre_images: &mut h.pre,
        snapshots: &mut h.snaps,
        executor: &mut h.exec,
        verifier: &mut h.verifier,
        rehearsal: &mut h.rehearsal,
        approval: &mut h.approval,
    };
    let faults = NoFaults;
    let mut engine =
        spot_fix_pipeline::FixEngine::new(policy, &h.journal, &mut h.audit, ports, &faults);
    let error = engine.run(&fix, &plan, &ctx).unwrap_err();
    assert!(matches!(error, FixError::InvalidPlan { .. }));
    assert!(h.records(&fix).is_empty());
}

#[test]
fn a_large_small_tier_fix_takes_a_snapshot_and_holds_the_gate() {
    let targets: Vec<(String, i128)> = (0..60)
        .map(|index| (format!("job:{index}"), i128::from(index)))
        .collect();
    let borrow: Vec<(&str, i128)> = targets
        .iter()
        .map(|(name, value)| (name.as_str(), *value))
        .collect();
    let mut h = Harness::with_options(
        &borrow,
        Tier::T1,
        FixPolicy::default().with_rehearsal_optional(common::REMEDY),
        FakeOptions::default(),
    );
    let fix = fix_id();
    let outcome = h.run(&fix, &NoFaults).unwrap();
    assert_eq!(outcome.state, FixState::Verified);
    assert_eq!(h.policy.mechanism(&h.plan), Mechanism::Snapshot);
    let labels = labels(&h.records(&fix));
    assert!(labels.contains(&"snapshot_taken"));
}

#[test]
fn a_remedy_dry_run_normalises_to_a_sealed_fix_plan() {
    let meta = RemedyMeta {
        title: "Retry a failed job".to_owned(),
        tier: Tier::T2,
        risk: spot_remedies::Risk::Medium,
        affects: Vec::new(),
        params: spot_remedies::ParamSchema::empty(),
        preconditions: "the job is failed".to_owned(),
        undo: UndoKind::reversible("requeue the job"),
    };
    let plan = RemedyPlan::seal(
        RemedyId::new("job.retry_failed"),
        serde_json::json!({}),
        vec![
            PlanStep::new("job:17", "job.retry", "retry the failed job"),
            PlanStep::new("job:18", "job.retry", "retry the failed job"),
        ],
    );
    let fix = FixPlan::from_remedy(&meta, &plan).unwrap();

    assert_eq!(fix.tier, Tier::T2);
    assert_eq!(fix.risk, Risk::Medium);
    assert!(fix.reversible);
    assert_eq!(fix.affected, vec!["job:17".to_owned(), "job:18".to_owned()]);
    assert_eq!(fix.steps.len(), 2);
    assert!(fix.verify());
    assert_eq!(fix.remedy().map(RemedyId::as_str), Some("job.retry_failed"));
}

#[test]
fn close_expired_closes_the_window_and_undo_then_refuses() {
    let mut h = Harness::small();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();

    let expires_at = h.progress(&fix).undo.unwrap().expires_at;
    assert!(h.close_expired(&fix, expires_at + 1).unwrap());
    assert!(!h.close_expired(&fix, expires_at + 2).unwrap());
    let labels = labels(&h.records(&fix));
    assert!(labels.contains(&"undo_window_closed"));

    let error = h.undo(&fix, expires_at + 1).unwrap_err();
    assert!(matches!(error, FixError::UndoExpired { .. }));
}

#[test]
fn an_app_install_plan_normalises_to_a_t3_structural_plan() {
    let plan = FixPlan::from_app_install(
        "install-plan-hash",
        vec![
            FixStep::new(
                0,
                "install-0",
                "app:demo",
                "app.install",
                "install the demo app",
            ),
            FixStep::new(
                1,
                "install-1",
                "app:other",
                "app.upgrade",
                "upgrade the other app",
            ),
        ],
        Risk::High,
        false,
    )
    .unwrap();
    assert_eq!(plan.tier, Tier::T3);
    assert_eq!(plan.risk, Risk::High);
    assert!(!plan.reversible);
    assert!(plan.verify());
    assert!(matches!(
        plan.source,
        FixSource::Structural {
            structural: StructuralKind::AppInstall,
            ..
        }
    ));
}

/// An audit sink that always fails, to prove the audit log cannot be disabled.
struct FailingAudit;

impl spot_fix_pipeline::FixAudit for FailingAudit {
    fn record(
        &mut self,
        _event: spot_fix_pipeline::FixAuditEvent,
    ) -> spot_fix_pipeline::Result<()> {
        Err(FixError::AuditFailed {
            detail: "the sink is down".to_owned(),
        })
    }
}

#[test]
fn the_audit_log_cannot_be_disabled() {
    let mut h = Harness::small();
    let fix = fix_id();
    let plan = h.plan.clone();
    let ctx = h.ctx.clone();
    let policy = h.policy.clone();
    let mut audit = FailingAudit;
    let ports = spot_fix_pipeline::FixPorts {
        pre_images: &mut h.pre,
        snapshots: &mut h.snaps,
        executor: &mut h.exec,
        verifier: &mut h.verifier,
        rehearsal: &mut h.rehearsal,
        approval: &mut h.approval,
    };
    let faults = NoFaults;
    let mut engine =
        spot_fix_pipeline::FixEngine::new(policy, &h.journal, &mut audit, ports, &faults);
    let error = engine.run(&fix, &plan, &ctx).unwrap_err();
    assert!(matches!(error, FixError::AuditFailed { .. }));
    // Only the proposal was journaled: the run stopped at the first unrecordable transition and
    // nothing was applied.
    let labels = labels(&h.records(&fix));
    assert_eq!(labels, vec!["proposed"]);
    assert_eq!(h.world.snapshot().unwrap(), h.pre_state());
}

#[test]
fn a_t3_structural_plan_is_always_rehearsed_even_when_low_risk() {
    let plan = FixPlan::seal(
        FixSource::Structural {
            structural: StructuralKind::AppInstall,
            plan_hash: "install".to_owned(),
        },
        Tier::T3,
        vec![FixStep::new(
            0,
            "s0",
            "app:demo",
            "app.install",
            "install the app",
        )],
        Risk::Low,
        true,
        Vec::new(),
    )
    .unwrap();
    let policy = FixPolicy::default().with_rehearsal_optional("job.retry_failed");
    assert_eq!(policy.path(&plan), spot_fix_pipeline::PlanPath::Rehearse);
    assert!(policy.requires_approval(
        &plan,
        &spot_fix_pipeline::AgentRef::new("agent-x", Tier::T3, false)
    ));
}
