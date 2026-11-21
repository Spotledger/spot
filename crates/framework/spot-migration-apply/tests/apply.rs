//! The apply engine end to end on the fakes: the happy path, the entry refusals, a failed
//! verification, the maintenance gate, the side-effect hold, progress and the journal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_migration_apply::testing::{FakeApplied, FakeRunner, StepConfig, sample_plan, ticket_for};
use spot_migration_apply::{
    AdmissionGate, ApplyOutcome, ApplyRequest, ApplyState, GateStatus, PlanVerifier, RestoreCause,
    SnapshotId, StaticInvariantSet, Version, Violation,
};
use spot_migration_plan::Area;
use support::{Harness, step_ids};

/// A plan with one step of each kind: schema, metadata publication, patch and behaviour.
fn four_step_plan() -> spot_migration_plan::ReleasePlan {
    sample_plan(&[Area::Schema, Area::Metadata, Area::Patch, Area::Behaviour])
}

/// Every step idempotent, the schema and patch steps transactional.
fn configured_runner(runner: FakeRunner) -> FakeRunner {
    runner
        .configure(0, StepConfig::transactional())
        .configure(2, StepConfig::transactional())
}

#[tokio::test]
async fn happy_path_opens_the_new_version_after_verify() {
    let plan = four_step_plan();
    assert_eq!(plan.steps().len(), 4, "one step of each kind");
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    let engine = harness.engine();

    let outcome = engine
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();

    assert_eq!(
        outcome,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true
        }
    );
    assert_eq!(harness.site.applied(), step_ids(&plan));
    assert_eq!(harness.gate.status(), GateStatus::Open);
    assert!(matches!(
        engine.state(),
        ApplyState::Open {
            version: Version::New,
            verified: true
        }
    ));
    // The hold was begun before the first step and released after verify.
    let log = harness.hold.log();
    assert!(log.first().unwrap().starts_with("begin:"), "{log:?}");
    assert!(log.last().unwrap().starts_with("release:"), "{log:?}");
}

#[tokio::test]
async fn the_gate_is_closed_before_any_step_and_opened_only_after_verify() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    harness
        .engine()
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();

    let tags = harness.progress.tags();
    let gate_closed = tags.iter().position(|tag| *tag == "gate_closed").unwrap();
    let first_step = tags.iter().position(|tag| *tag == "step_started").unwrap();
    let verify_passed = tags.iter().position(|tag| *tag == "verify_passed").unwrap();
    let opened = tags.iter().position(|tag| *tag == "opened").unwrap();
    assert!(gate_closed < first_step, "{tags:?}");
    assert!(verify_passed < opened, "{tags:?}");
    assert_eq!(tags.last(), Some(&"opened"), "{tags:?}");
}

#[tokio::test]
async fn no_ticket_is_refused() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    let engine = harness.engine();
    let request = ApplyRequest::new(plan.clone(), None, Some(snapshot), 1_000);
    let error = engine.apply(request).await.unwrap_err();
    assert_eq!(error.code(), "migration_apply.no_ticket");
    assert_eq!(harness.gate.status(), GateStatus::Open, "nothing started");
}

#[tokio::test]
async fn no_snapshot_is_refused() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let engine = harness.engine();
    let request = ApplyRequest::new(
        plan.clone(),
        Some(ticket_for(&plan, 1_000).unwrap()),
        None,
        1_000,
    );
    let error = engine.apply(request).await.unwrap_err();
    assert_eq!(error.code(), "migration_apply.no_snapshot");
}

#[tokio::test]
async fn an_untaken_snapshot_is_refused() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let engine = harness.engine();
    let request = ApplyRequest::new(
        plan.clone(),
        Some(ticket_for(&plan, 1_000).unwrap()),
        Some(SnapshotId::new("snap-999").unwrap()),
        1_000,
    );
    let error = engine.apply(request).await.unwrap_err();
    assert_eq!(error.code(), "migration_apply.no_snapshot");
}

#[tokio::test]
async fn a_ticket_for_another_plan_is_refused() {
    let plan = four_step_plan();
    let other = sample_plan(&[Area::Schema]);
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    let engine = harness.engine();
    let request = ApplyRequest::new(
        plan.clone(),
        Some(ticket_for(&other, 1_000).unwrap()),
        Some(snapshot),
        1_000,
    );
    let error = engine.apply(request).await.unwrap_err();
    assert_eq!(error.code(), "migration_apply.ticket_mismatch");
}

#[tokio::test]
async fn a_failed_verify_restores_the_old_version_and_discards_the_hold() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    harness.verify.set_pass(false);
    let snapshot = harness.take().await;
    let engine = harness.engine();

    let outcome = engine
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();

    assert_eq!(
        outcome,
        ApplyOutcome::Restored {
            cause: RestoreCause::VerifyFailed,
            mechanism: spot_migration_apply::RestoreMechanism::Snapshot
        }
    );
    assert!(
        harness.site.applied().is_empty(),
        "restored to the snapshot"
    );
    assert_eq!(harness.gate.status(), GateStatus::Open);
    assert!(matches!(
        engine.state(),
        ApplyState::Open {
            version: Version::Old,
            verified: false
        }
    ));
    let log = harness.hold.log();
    assert!(log.last().unwrap().starts_with("discard:"), "{log:?}");
}

#[tokio::test]
async fn a_failing_step_restores_the_old_version() {
    let plan = four_step_plan();
    let harness = Harness::new(|runner| {
        configured_runner(runner).configure(1, {
            StepConfig {
                fail: true,
                ..StepConfig::default()
            }
        })
    });
    let snapshot = harness.take().await;
    let engine = harness.engine();

    let outcome = engine
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        ApplyOutcome::Restored {
            cause: RestoreCause::StepFailed { index: 1 },
            ..
        }
    ));
    assert!(harness.site.applied().is_empty());
    assert_eq!(harness.gate.status(), GateStatus::Open);
}

#[tokio::test]
async fn the_plan_verifier_passes_when_the_target_matches_and_fails_on_a_violation() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    // The applied system equals the plan's target: no residual difference.
    let verifier = PlanVerifier::new(Arc::new(FakeApplied::new(plan.to.clone())));
    let engine = harness.engine_with_verify(Arc::new(verifier));
    let outcome = engine
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();
    assert!(matches!(outcome, ApplyOutcome::Opened { .. }));

    // A fresh run whose invariant set fails must restore instead.
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    let verifier =
        PlanVerifier::new(Arc::new(FakeApplied::new(plan.to.clone()))).with_invariants(vec![
            Arc::new(StaticInvariantSet::new(
                "l7",
                vec![Violation::new("balance", "trial balance does not sum")],
            )),
        ]);
    let engine = harness.engine_with_verify(Arc::new(verifier));
    let outcome = engine
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();
    assert!(matches!(outcome, ApplyOutcome::Restored { .. }));
    assert!(harness.site.applied().is_empty());
}

#[tokio::test]
async fn the_journal_records_every_step_before_it_completes() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    harness
        .engine()
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();

    let records = harness.journal.written();
    // Every `StepStarted` comes before its `StepCompleted`.
    for index in 0..4 {
        let started = records.iter().position(|entry| {
            matches!(
                entry.record,
                spot_migration_apply::JournalRecord::StepStarted { index: i, .. } if i == index
            )
        });
        let completed = records.iter().position(|entry| {
            matches!(
                entry.record,
                spot_migration_apply::JournalRecord::StepCompleted { index: i, .. } if i == index
            )
        });
        assert!(
            started < completed,
            "step {index} not write-ahead journaled"
        );
    }
    // The site was opened after verification was recorded.
    let verified = records
        .iter()
        .position(|entry| matches!(entry.record, spot_migration_apply::JournalRecord::Verified));
    let opened = records.iter().position(|entry| {
        matches!(
            entry.record,
            spot_migration_apply::JournalRecord::GateOpened { .. }
        )
    });
    assert!(verified < opened);
}

#[tokio::test]
async fn the_drain_deadline_halts_with_the_site_closed() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    // A transaction is still in flight, and it never finishes.
    harness.gate.set_inflight(1);
    let engine = harness
        .engine()
        .with_options(spot_migration_apply::ApplyOptions {
            drain_deadline_micros: 2_000,
            ..spot_migration_apply::ApplyOptions::default()
        });

    let error = engine
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();

    assert_eq!(error.code(), "migration_apply.drain_timeout");
    assert!(!harness.gate.status().is_open(), "the site stays closed");
    assert!(matches!(engine.state(), ApplyState::Halted { .. }));
    assert!(harness.site.applied().is_empty(), "no step ran");
}

#[tokio::test]
async fn a_snapshot_restore_failure_leaves_the_site_closed() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    harness.verify.set_pass(false);
    harness.snapshot.fail_restores();
    let snapshot = harness.take().await;
    let engine = harness.engine();

    let error = engine
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();

    assert_eq!(error.code(), "migration_apply.snapshot_failed");
    assert!(!harness.gate.status().is_open(), "the site stays closed");
}

#[tokio::test]
async fn an_empty_plan_verifies_and_opens() {
    let plan = sample_plan(&[]);
    let harness = Harness::new(|runner| runner);
    let snapshot = harness.take().await;
    let engine = harness.engine();
    let outcome = engine
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();
    assert!(matches!(outcome, ApplyOutcome::Opened { .. }));
    assert_eq!(harness.gate.status(), GateStatus::Open);
}

#[tokio::test]
async fn the_trait_object_applies_and_reports_state() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    let engine = harness.engine();
    let service: &dyn spot_migration_apply::ApplyService = &engine;
    let outcome = service
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();
    assert!(matches!(outcome, ApplyOutcome::Opened { .. }));
    assert!(service.state().admits_users());
    // A second apply of a different plan is allowed once the first finished.
    let other = sample_plan(&[Area::Schema]);
    let snapshot = harness.take().await;
    let outcome = service
        .apply(harness.request(&other, snapshot, 2_000))
        .await
        .unwrap();
    assert!(matches!(outcome, ApplyOutcome::Opened { .. }));
}

#[tokio::test]
async fn a_verified_plan_is_never_applied_twice_with_the_same_ticket() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    let engine = harness.engine();
    engine
        .apply(harness.request(&plan, snapshot.clone(), 1_000))
        .await
        .unwrap();
    let again = engine
        .apply(harness.request(&plan, snapshot, 2_000))
        .await
        .unwrap_err();
    assert_eq!(again.code(), spot_migration_apply::codes::ALREADY_APPLIED);
    assert_eq!(harness.site.applied(), step_ids(&plan), "no step ran twice");
}
