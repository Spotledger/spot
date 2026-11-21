//! The rehearsal engine end to end, on fakes: a no-op release has an empty diff, a seeded change
//! is unexplained, a declared change is expected, and the clone is destroyed on every exit path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_migration_plan::Change;
use spot_rehearsal::{
    CleanupOutcome, DiffClass, InvariantViolation, Rehearsal, RehearsalOutcome, ReleaseDeclaration,
    StaticInvariantSet, codes,
};
use spot_storage_contract::Value;
use support::*;

/// A world with two trial-balance rows: cash/acme debit 100 and bank/acme debit 200.
fn world_with_rows() -> Arc<FakeWorld> {
    let world = FakeWorld::new();
    world.set(
        "trial_balance",
        &["account", "company", "debit"],
        vec![
            vec![
                Value::Text("cash".to_owned()),
                Value::Text("acme".to_owned()),
                Value::Int(100),
            ],
            vec![
                Value::Text("bank".to_owned()),
                Value::Text("acme".to_owned()),
                Value::Int(200),
            ],
        ],
    );
    world
}

#[tokio::test]
async fn a_no_op_release_has_an_empty_diff() {
    let world = world_with_rows();
    let reaper = Arc::new(FakeScratch::new());
    let dir = tempdir("no-op");
    let rehearsal = Rehearsal::new(
        Arc::new(FakeSnapshotProvider::ok()),
        Arc::new(FakeApplier::noop(world.clone())),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(dir.clone(), Some(reaper.clone())),
    )
    .with_invariants(vec![Arc::new(StaticInvariantSet::passing("framework"))]);

    let plan = empty_plan();
    let report = rehearsal
        .run(&plan, &ticket(&plan), &site())
        .await
        .expect("rehearsal runs");

    assert!(report.diffs.is_empty(), "no difference expected");
    assert_eq!(report.pre_digest, report.post_digest);
    assert_eq!(report.outcome, RehearsalOutcome::Clean);
    assert_eq!(report.invariants.total(), 0);
    assert_eq!(reaper.destroyed(), 1, "the scratch was destroyed");
    assert!(report.clone.cleanup.cleaned());
    assert!(!dir.exists(), "the snapshot directory was removed");
    assert_eq!(report.apply.plan_digest, plan.plan_digest);
}

#[tokio::test]
async fn a_seeded_change_is_unexplained() {
    let world = world_with_rows();
    let reaper = Arc::new(FakeScratch::new());
    let dir = tempdir("seeded");
    let rehearsal = Rehearsal::new(
        Arc::new(FakeSnapshotProvider::ok()),
        // Round the cash row's debit by +1.
        Arc::new(FakeApplier::seeded(
            world.clone(),
            ("trial_balance".to_owned(), 0, 2, 1),
        )),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(dir.clone(), Some(reaper.clone())),
    );

    let plan = empty_plan();
    let report = rehearsal
        .run(&plan, &ticket(&plan), &site())
        .await
        .expect("rehearsal runs");

    assert_eq!(report.diffs.len(), 1, "exactly the seeded change");
    assert_eq!(report.diffs[0].class, DiffClass::Unexplained);
    assert_eq!(report.diffs[0].before, 100);
    assert_eq!(report.diffs[0].after, 101);
    assert_eq!(report.diffs[0].delta, 1);
    assert_eq!(report.outcome, RehearsalOutcome::UnexplainedDifferences);
    assert_ne!(report.pre_digest, report.post_digest);
    assert_eq!(reaper.destroyed(), 1);
}

/// A declaration that says one change moves the trial balance.
struct DeclaresTrialBalance;

impl ReleaseDeclaration for DeclaresTrialBalance {
    fn is_behaviour_change(&self, _change: &Change) -> bool {
        true
    }

    fn is_high_sensitivity(&self, _change: &Change) -> bool {
        false
    }

    fn expected_sections(&self, _change: &Change) -> Vec<String> {
        vec!["trial_balance".to_owned()]
    }
}

#[tokio::test]
async fn a_declared_change_is_expected() {
    let world = world_with_rows();
    let reaper = Arc::new(FakeScratch::new());
    let dir = tempdir("declared");
    let rehearsal = Rehearsal::new(
        Arc::new(FakeSnapshotProvider::ok()),
        Arc::new(FakeApplier::seeded(
            world.clone(),
            ("trial_balance".to_owned(), 0, 2, 5),
        )),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(dir.clone(), Some(reaper.clone())),
    )
    .with_declaration(Arc::new(DeclaresTrialBalance));

    let plan = plan_with_behaviour_change("behaviour/rounding");
    let report = rehearsal
        .run(&plan, &ticket(&plan), &site())
        .await
        .expect("rehearsal runs");

    assert_eq!(report.diffs.len(), 1);
    assert_eq!(report.diffs[0].class, DiffClass::Expected);
    assert_eq!(
        report.diffs[0].explained_by.as_deref(),
        Some("behaviour/rounding")
    );
    assert_eq!(report.outcome, RehearsalOutcome::ExpectedChanges);
}

#[tokio::test]
async fn a_failed_invariant_wins_over_a_clean_diff() {
    let world = world_with_rows();
    let reaper = Arc::new(FakeScratch::new());
    let dir = tempdir("invariant");
    let violation = InvariantViolation::new(
        "framework",
        "seq-gapless",
        "sequence",
        "-",
        "expected=3 actual=2",
    );
    let rehearsal = Rehearsal::new(
        Arc::new(FakeSnapshotProvider::ok()),
        Arc::new(FakeApplier::noop(world.clone())),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(dir.clone(), Some(reaper.clone())),
    )
    .with_invariants(vec![Arc::new(StaticInvariantSet::new(
        "framework",
        vec![violation],
    ))]);

    let plan = empty_plan();
    let report = rehearsal
        .run(&plan, &ticket(&plan), &site())
        .await
        .expect("rehearsal runs");

    assert_eq!(report.outcome, RehearsalOutcome::InvariantsFailed);
    assert_eq!(report.invariants.failed_sets(), vec!["framework"]);
    assert_eq!(reaper.destroyed(), 1);
}

#[tokio::test]
async fn a_ticket_for_another_plan_is_refused_before_any_clone_is_made() {
    let world = world_with_rows();
    let provider = Arc::new(FakeSnapshotProvider::ok());
    let reaper = Arc::new(FakeScratch::new());
    let rehearsal = Rehearsal::new(
        provider.clone(),
        Arc::new(FakeApplier::noop(world.clone())),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(tempdir("ticket"), Some(reaper.clone())),
    );
    let plan = empty_plan();
    let other = plan_with_behaviour_change("other");
    let error = rehearsal
        .run(&plan, &ticket(&other), &site())
        .await
        .expect_err("refused");
    assert_eq!(error.code(), codes::TICKET_MISMATCH);
    assert_eq!(provider.takes(), 0, "no snapshot was taken");
    assert_eq!(provider.restores(), 0, "no clone was restored");
    assert_eq!(reaper.destroyed(), 0);
}

#[tokio::test]
async fn a_plan_modified_after_approval_is_refused() {
    let world = world_with_rows();
    let provider = Arc::new(FakeSnapshotProvider::ok());
    let rehearsal = Rehearsal::new(
        provider.clone(),
        Arc::new(FakeApplier::noop(world.clone())),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(tempdir("modified"), None),
    );
    let mut plan = plan_with_behaviour_change("a");
    let approved = ticket(&plan);
    plan.changes
        .push(plan_with_behaviour_change("b").changes.remove(0));
    let error = rehearsal
        .run(&plan, &approved, &site())
        .await
        .expect_err("refused");
    assert_eq!(error.code(), codes::TICKET_MISMATCH);
    assert_eq!(provider.takes(), 0);
}

#[tokio::test]
async fn a_failed_drop_is_visible_in_the_report() {
    let world = world_with_rows();
    let reaper = Arc::new(FakeScratch::failing());
    let rehearsal = Rehearsal::new(
        Arc::new(FakeSnapshotProvider::ok()),
        Arc::new(FakeApplier::noop(world.clone())),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(tempdir("drop-fail"), Some(reaper.clone())),
    );
    let plan = empty_plan();
    let report = rehearsal
        .run(&plan, &ticket(&plan), &site())
        .await
        .expect("rehearsal runs");
    assert!(!report.clone.cleanup.cleaned());
    assert!(
        matches!(&report.clone.cleanup, CleanupOutcome::Failed { reason } if reason.contains("drop refused")),
        "{:?}",
        report.clone.cleanup
    );
    assert_eq!(reaper.destroyed(), 1, "dropped once, not again by Drop");
}

#[tokio::test]
async fn the_clone_is_destroyed_when_the_take_fails() {
    let world = world_with_rows();
    let reaper = Arc::new(FakeScratch::new());
    let dir = tempdir("take-fail");
    let rehearsal = Rehearsal::new(
        Arc::new(FakeSnapshotProvider::failing_take_after_write()),
        Arc::new(FakeApplier::noop(world.clone())),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(dir.clone(), Some(reaper.clone())),
    );
    let plan = empty_plan();
    let error = rehearsal
        .run(&plan, &ticket(&plan), &site())
        .await
        .expect_err("take failed");
    assert_eq!(error.code(), codes::SNAPSHOT_FAILED);
    assert_eq!(reaper.destroyed(), 1);
    assert!(!dir.exists(), "the partial snapshot directory was removed");
}

#[tokio::test]
async fn the_clone_is_destroyed_when_the_restore_fails() {
    let world = world_with_rows();
    let reaper = Arc::new(FakeScratch::new());
    let dir = tempdir("restore-fail");
    let rehearsal = Rehearsal::new(
        Arc::new(FakeSnapshotProvider::failing_restore()),
        Arc::new(FakeApplier::noop(world.clone())),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(dir.clone(), Some(reaper.clone())),
    );
    let plan = empty_plan();
    let error = rehearsal
        .run(&plan, &ticket(&plan), &site())
        .await
        .expect_err("restore failed");
    assert_eq!(error.code(), codes::SNAPSHOT_FAILED);
    assert_eq!(reaper.destroyed(), 1);
    assert!(!dir.exists());
}

#[tokio::test]
async fn the_clone_is_destroyed_when_the_apply_fails() {
    let world = world_with_rows();
    let reaper = Arc::new(FakeScratch::new());
    let dir = tempdir("apply-fail");
    let rehearsal = Rehearsal::new(
        Arc::new(FakeSnapshotProvider::ok()),
        Arc::new(FakeApplier::failing(world.clone())),
        Arc::new(FakeFingerprintSource::new(world.clone())),
        spec(),
        clone_request(dir.clone(), Some(reaper.clone())),
    );
    let plan = empty_plan();
    let error = rehearsal
        .run(&plan, &ticket(&plan), &site())
        .await
        .expect_err("apply failed");
    assert_eq!(error.code(), codes::APPLY_FAILED);
    assert_eq!(reaper.destroyed(), 1);
    assert!(!dir.exists());
}

/// A directory under the system temp dir, unique to the test name.
fn tempdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("spot-rehearsal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
