//! Recovery: a kill at every journal boundary and inside every step kind, and the rule that a
//! restart either resumes cleanly or restores cleanly — never opens an unverified new version.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_migration_apply::testing::{FakeRunner, StepConfig, sample_plan};
use spot_migration_apply::{
    AdmissionGate, ApplyOutcome, ApplyState, GateStatus, KillPoint, RecoveryAction, RestoreCause,
    Version,
};
use spot_migration_plan::{Area, ReleasePlan};
use support::{Harness, step_ids};

fn four_step_plan() -> ReleasePlan {
    sample_plan(&[Area::Schema, Area::Metadata, Area::Patch, Area::Behaviour])
}

fn configured_runner(runner: FakeRunner) -> FakeRunner {
    runner
        .configure(0, StepConfig::transactional())
        .configure(2, StepConfig::transactional())
}

/// The end-state invariant: the site admits users, and a `New` version is always verified.
fn assert_clean(harness: &Harness, plan: &ReleasePlan, state: &ApplyState, what: &str) {
    assert!(
        harness.gate.status().is_open(),
        "{what}: the gate is not open at the end"
    );
    match state {
        ApplyState::Open {
            version: Version::New,
            verified,
        } => {
            assert!(verified, "{what}: a new version was opened unverified");
            assert_eq!(
                harness.site.applied(),
                step_ids(plan),
                "{what}: the new version is not fully applied"
            );
        }
        ApplyState::Open {
            version: Version::Old,
            ..
        } => {
            assert!(
                harness.site.applied().is_empty(),
                "{what}: the old version is not the snapshot"
            );
        }
        ApplyState::Idle => {}
        other => panic!("{what}: unexpected end state {other:?}"),
    }
}

#[tokio::test]
async fn a_kill_at_every_journal_boundary_is_clean() {
    let plan = four_step_plan();

    // How many journal writes a full successful run makes.
    let baseline = Harness::new(configured_runner);
    let snapshot = baseline.take().await;
    baseline
        .engine()
        .apply(baseline.request(&plan, snapshot, 1_000))
        .await
        .unwrap();
    let writes = baseline.journal.written().len() as u64;

    let mut points: Vec<KillPoint> = Vec::new();
    for seq in 1..=writes {
        points.push(KillPoint::BeforeJournal(seq));
        points.push(KillPoint::AfterJournal(seq));
    }
    for index in 0..plan.steps().len() {
        points.push(KillPoint::BeforeStep(index));
        points.push(KillPoint::AfterStep(index));
    }
    points.push(KillPoint::BeforeVerify);
    points.push(KillPoint::AfterVerify);

    for point in points {
        let what = format!("kill at {}", point.label());
        let harness = Harness::new(configured_runner);
        let snapshot = harness.take().await;
        let engine = harness.engine_killing_at(point.clone());
        match engine.apply(harness.request(&plan, snapshot, 1_000)).await {
            Ok(_) => {
                assert_clean(&harness, &plan, &engine.state(), &what);
            }
            Err(error) if error.is_killed() => {
                if support::gate_must_be_closed(&harness) {
                    assert!(
                        !harness.gate.status().is_open(),
                        "{what}: the killed run left the gate open"
                    );
                }
                let recovery = harness
                    .engine()
                    .recover(&plan, 2_000)
                    .await
                    .unwrap_or_else(|error| panic!("{what}: recover failed: {error}"));
                assert_clean(&harness, &plan, &recovery.state, &what);
            }
            Err(error) => panic!("{what}: unexpected error {error}"),
        }
    }
}

#[tokio::test]
async fn a_kill_inside_every_step_kind_is_clean() {
    let plan = four_step_plan();
    for index in 0..plan.steps().len() {
        let transactional = index == 0 || index == 2;
        let harness = Harness::new(move |runner| {
            let base = if transactional {
                StepConfig::transactional()
            } else {
                StepConfig::default()
            };
            configured_runner(runner).configure(index, StepConfig { kill: true, ..base })
        });
        let snapshot = harness.take().await;
        let error = harness
            .engine()
            .apply(harness.request(&plan, snapshot, 1_000))
            .await
            .unwrap_err();
        assert!(error.is_killed(), "step {index}: {error}");
        assert!(!harness.gate.status().is_open());
        let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
        assert_clean(
            &harness,
            &plan,
            &recovery.state,
            &format!("step {index} kill"),
        );
    }
}

#[tokio::test]
async fn a_transactional_idempotent_step_resumes_after_a_kill() {
    let plan = four_step_plan();
    let harness = Harness::new(|runner| {
        configured_runner(runner).configure(
            0,
            StepConfig {
                kill: true,
                ..StepConfig::transactional()
            },
        )
    });
    let snapshot = harness.take().await;
    let error = harness
        .engine()
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();
    assert!(error.is_killed());

    let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
    assert!(
        matches!(recovery.action, RecoveryAction::Resumed { from_step: 0 }),
        "{:?}",
        recovery.action
    );
    assert!(matches!(
        recovery.state,
        ApplyState::Open {
            version: Version::New,
            verified: true
        }
    ));
    assert_eq!(harness.site.applied(), step_ids(&plan));
    // The transactional step was killed once and resumed once: the transaction aborted, so the
    // first execution never committed.
    assert_eq!(harness.runner.run_count(0), 2);
}

#[tokio::test]
async fn a_non_transactional_step_rolls_back_to_its_checkpoint_and_resumes() {
    let plan = four_step_plan();
    let harness = Harness::new(|runner| {
        configured_runner(runner).configure(
            1,
            StepConfig {
                kill: true,
                ..StepConfig::default()
            },
        )
    });
    let snapshot = harness.take().await;
    let error = harness
        .engine()
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();
    assert!(error.is_killed());

    let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
    assert!(matches!(
        recovery.action,
        RecoveryAction::Resumed { from_step: 1 }
    ));
    assert!(matches!(
        recovery.state,
        ApplyState::Open {
            version: Version::New,
            verified: true
        }
    ));
    // The partial effect was compensated by the checkpoint, so the step is present exactly once.
    assert_eq!(harness.site.applied(), step_ids(&plan));
    assert_eq!(harness.runner.run_count(1), 2);
}

#[tokio::test]
async fn an_undeclared_step_is_never_run_twice() {
    let plan = four_step_plan();
    let harness = Harness::new(|runner| {
        configured_runner(runner).configure(
            1,
            StepConfig {
                kill: true,
                ..StepConfig::undeclared()
            },
        )
    });
    let snapshot = harness.take().await;
    let error = harness
        .engine()
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();
    assert!(error.is_killed());

    let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
    assert_eq!(recovery.action, RecoveryAction::Restored);
    assert!(matches!(
        recovery.state,
        ApplyState::Open {
            version: Version::Old,
            verified: false
        }
    ));
    // The undeclared step ran once and was not run again; the snapshot restored the site.
    assert_eq!(harness.runner.run_count(1), 1);
    assert!(harness.site.applied().is_empty());
}

#[tokio::test]
async fn an_interrupted_snapshot_restore_is_finished_on_recovery() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    harness.verify.set_pass(false);
    let snapshot = harness.take().await;
    let error = harness
        .engine_killing_at(KillPoint::BeforeRestore)
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();
    assert!(error.is_killed());
    assert!(!harness.gate.status().is_open());

    let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
    assert_eq!(recovery.action, RecoveryAction::Restored);
    assert!(
        harness.site.applied().is_empty(),
        "the restore was finished"
    );
    assert!(matches!(
        recovery.state,
        ApplyState::Open {
            version: Version::Old,
            verified: false
        }
    ));
}

#[tokio::test]
async fn a_kill_after_verify_opens_the_new_version() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    let error = harness
        .engine_killing_at(KillPoint::AfterVerify)
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();
    assert!(error.is_killed());
    assert!(!harness.gate.status().is_open());

    let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
    assert!(matches!(
        recovery.state,
        ApplyState::Open {
            version: Version::New,
            verified: true
        }
    ));
    assert_eq!(harness.site.applied(), step_ids(&plan));
    assert!(harness.hold.held().is_none(), "the hold was released");
}

#[tokio::test]
async fn recovery_with_no_journal_did_not_start() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
    assert_eq!(recovery.action, RecoveryAction::NotStarted);
    assert_eq!(recovery.state, ApplyState::Idle);
    assert_eq!(harness.gate.status(), GateStatus::Open);
}

#[tokio::test]
async fn recovery_of_an_open_run_reports_already_open() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    let outcome = harness
        .engine()
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap();
    assert!(matches!(outcome, ApplyOutcome::Opened { .. }));

    let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
    assert_eq!(
        recovery.action,
        RecoveryAction::AlreadyOpen {
            version: Version::New,
            verified: true
        }
    );
}

#[tokio::test]
async fn recovery_for_another_plan_is_refused() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    let snapshot = harness.take().await;
    harness
        .engine_killing_at(KillPoint::BeforeStep(1))
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();

    let other = sample_plan(&[Area::Schema, Area::Metadata, Area::Patch]);
    let error = harness.engine().recover(&other, 2_000).await.unwrap_err();
    assert_eq!(error.code(), "migration_apply.in_progress");
}

#[tokio::test]
async fn a_failed_step_after_recovery_still_restores() {
    let plan = four_step_plan();
    // Step 2 fails; a kill before step 1 makes the restart resume into that failure.
    let harness = Harness::new(|runner| {
        configured_runner(runner).configure(
            2,
            StepConfig {
                fail: true,
                ..StepConfig::transactional()
            },
        )
    });
    let snapshot = harness.take().await;
    harness
        .engine_killing_at(KillPoint::BeforeStep(1))
        .apply(harness.request(&plan, snapshot, 1_000))
        .await
        .unwrap_err();

    let recovery = harness.engine().recover(&plan, 2_000).await.unwrap();
    assert_eq!(recovery.action, RecoveryAction::Restored);
    assert!(matches!(
        recovery.state,
        ApplyState::Open {
            version: Version::Old,
            verified: false
        }
    ));
    assert!(harness.site.applied().is_empty());
}

#[tokio::test]
async fn the_restore_cause_is_recorded() {
    let plan = four_step_plan();
    let harness = Harness::new(configured_runner);
    harness.verify.set_pass(false);
    let snapshot = harness.take().await;
    let outcome = harness
        .engine()
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
}
