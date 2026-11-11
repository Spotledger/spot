//! Kill/crash tests: fault injection at every journal boundary, then a restart.
//!
//! A kill returns [`FixError::Killed`] and leaves the journal as the only witness. A fresh
//! engine either resumes (verifies an applied fix, finishes a rehearsal) or rolls back, and the
//! world is either fully fixed and verified or byte-identical to the pre-state by fingerprint.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::{FakeOptions, Harness, fix_id};
use spot_agent_tiers::Tier;
use spot_fix_pipeline::{Faults, FixError, FixPolicy, FixState, KillAt, KillPoint, NoFaults};

/// Every boundary a direct T1 run can reach.
fn happy_kill_points() -> Vec<KillPoint> {
    vec![
        KillPoint::BeforeProposed,
        KillPoint::AfterProposed,
        KillPoint::BeforePlanned,
        KillPoint::AfterPlanned,
        KillPoint::BeforeApply,
        KillPoint::BeforePreImages,
        KillPoint::AfterPreImages,
        KillPoint::BeforeStep(0),
        KillPoint::AfterStep(0),
        KillPoint::BeforeStep(1),
        KillPoint::AfterStep(1),
        KillPoint::BeforeApplied,
        KillPoint::AfterApplied,
        KillPoint::BeforeVerify,
        KillPoint::AfterVerify,
    ]
}

/// Every boundary a rollback can reach.
fn rollback_kill_points() -> Vec<KillPoint> {
    vec![
        KillPoint::BeforeVerify,
        KillPoint::BeforeRollback,
        KillPoint::BeforeResidue,
        KillPoint::AfterRollback,
    ]
}

#[test]
fn a_direct_fix_survives_a_kill_at_every_boundary_it_reaches() {
    for point in happy_kill_points() {
        let mut h = Harness::small();
        let fix = fix_id();
        let kill = KillAt::new(point.clone());
        let result = h.run(&fix, &kill);
        match result {
            Err(FixError::Killed { .. }) => {
                let outcome = h.recover(&fix).unwrap();
                match outcome.state {
                    FixState::Verified => {
                        assert_eq!(
                            h.world.snapshot().unwrap(),
                            h.post_state(),
                            "a verified recovery is fully fixed at {point:?}"
                        );
                    }
                    FixState::Proposed | FixState::RolledBack => {
                        assert_eq!(
                            h.world.snapshot().unwrap(),
                            h.pre_state(),
                            "a resumed or rolled-back recovery is byte-identical at {point:?}"
                        );
                    }
                    other => panic!("unexpected state {other} after a kill at {point:?}"),
                }
            }
            Ok(outcome) => {
                // The boundary is not on this plan's path (for example a rehearsal point on a
                // direct plan): the run simply completed.
                assert_eq!(outcome.state, FixState::Verified);
            }
            Err(other) => panic!("unexpected error at {point:?}: {other}"),
        }
    }
}

#[test]
fn a_failed_verification_survives_a_kill_at_every_rollback_boundary() {
    for point in rollback_kill_points() {
        let mut h = Harness::with_options(
            &[("job:1", 10), ("job:2", 20)],
            Tier::T1,
            FixPolicy::default().with_rehearsal_optional(common::REMEDY),
            FakeOptions::symptom_fails(),
        );
        let fix = fix_id();
        let kill = KillAt::new(point.clone());
        let result = h.run(&fix, &kill);
        if let Err(FixError::Killed { .. }) = result {
            let outcome = h.recover(&fix).unwrap();
            assert_eq!(
                outcome.state,
                FixState::RolledBack,
                "the recovery rolls back after a kill at {point:?}"
            );
            assert_eq!(h.world.snapshot().unwrap(), h.pre_state());
            assert!(outcome.residue.unwrap().is_empty());
        } else {
            // A point that was not reached still ends in a clean rollback.
            assert_eq!(result.unwrap().state, FixState::RolledBack);
        }
    }
}

#[test]
fn a_t3_fix_survives_a_kill_around_the_snapshot() {
    for point in [
        KillPoint::BeforeSnapshot,
        KillPoint::AfterSnapshot,
        KillPoint::BeforePreImages,
        KillPoint::AfterPreImages,
    ] {
        let mut h = Harness::structural();
        let fix = fix_id();
        let kill = KillAt::new(point.clone());
        let result = h.run(&fix, &kill);
        match result {
            Err(FixError::Killed { .. }) => {
                let outcome = h.recover(&fix).unwrap();
                assert!(
                    matches!(outcome.state, FixState::RolledBack | FixState::Verified),
                    "state {} after a kill at {point:?}",
                    outcome.state
                );
                if outcome.state == FixState::RolledBack {
                    assert_eq!(h.world.snapshot().unwrap(), h.pre_state());
                    assert!(outcome.residue.unwrap().is_empty());
                }
                assert!(
                    !h.snaps.gate_closed(),
                    "the recovery never leaves users held out"
                );
            }
            Ok(outcome) => assert_eq!(outcome.state, FixState::Verified),
            Err(other) => panic!("unexpected error at {point:?}: {other}"),
        }
    }
}

#[test]
fn a_kill_before_an_undo_finishes_or_leaves_the_verified_fix_in_place() {
    let mut h = Harness::small();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();

    let kill = KillAt::new(KillPoint::BeforeUndo);
    match h.undo_with(&fix, 1_010, &kill) {
        Err(FixError::Killed { .. }) => {
            let outcome = h.recover(&fix).unwrap();
            assert_eq!(outcome.state, FixState::Verified);
            assert_eq!(h.world.snapshot().unwrap(), h.post_state());
        }
        other => panic!("expected a kill, got {other:?}"),
    }
}

#[test]
fn a_kill_after_an_undo_leaves_the_undo_finished_on_recovery() {
    let mut h = Harness::small();
    let fix = fix_id();
    h.run(&fix, &NoFaults).unwrap();

    let kill = KillAt::new(KillPoint::AfterUndo);
    match h.undo_with(&fix, 1_010, &kill) {
        Err(FixError::Killed { .. }) => {
            let outcome = h.recover(&fix).unwrap();
            assert_eq!(outcome.state, FixState::RolledBack);
            assert_eq!(h.world.snapshot().unwrap(), h.pre_state());
            assert!(outcome.residue.unwrap().is_empty());
        }
        other => panic!("expected a kill, got {other:?}"),
    }
}

#[test]
fn a_kill_after_a_rehearsal_resumes_the_apply() {
    let mut h = Harness::with_options(
        &[("job:1", 10), ("job:2", 20)],
        Tier::T2,
        FixPolicy::default(),
        FakeOptions::default(),
    );
    let fix = fix_id();
    let kill = KillAt::new(KillPoint::AfterRehearsal);
    let result = h.run(&fix, &kill);
    assert!(matches!(result, Err(FixError::Killed { .. })));
    let outcome = h.recover(&fix).unwrap();
    assert_eq!(outcome.state, FixState::Verified);
    assert_eq!(h.world.snapshot().unwrap(), h.post_state());
}

#[test]
fn no_faults_never_kills_and_a_kill_is_deterministic() {
    let none = NoFaults;
    assert!(none.at(&KillPoint::BeforeApply).is_ok());
    let kill = KillAt::new(KillPoint::BeforeApply);
    assert_eq!(kill.point(), &KillPoint::BeforeApply);
    assert!(matches!(
        kill.at(&KillPoint::BeforeApply),
        Err(FixError::Killed { .. })
    ));
    assert!(kill.at(&KillPoint::AfterApplied).is_ok());
    assert_eq!(KillPoint::BeforeStep(3).label(), "before_step:3".to_owned());
}
