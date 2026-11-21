//! Property test: any combination of steps, an injected step failure, a failed verification and
//! a random kill boundary ends either fully fixed and verified, or byte-identical to the
//! pre-state by fingerprint.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::{FakeOptions, Harness};
use proptest::prelude::*;
use spot_agent_tiers::Tier;
use spot_fix_pipeline::{FixError, FixPolicy, FixState, KillAt, KillPoint};

/// Every kill boundary a run can carry.
fn kill_points(step_count: usize) -> Vec<KillPoint> {
    let mut points = vec![
        KillPoint::BeforeProposed,
        KillPoint::AfterProposed,
        KillPoint::BeforePlanned,
        KillPoint::AfterPlanned,
        KillPoint::BeforeRehearsal,
        KillPoint::AfterRehearsal,
        KillPoint::BeforeApply,
        KillPoint::BeforeSnapshot,
        KillPoint::AfterSnapshot,
        KillPoint::BeforePreImages,
        KillPoint::AfterPreImages,
        KillPoint::BeforeApplied,
        KillPoint::AfterApplied,
        KillPoint::BeforeVerify,
        KillPoint::AfterVerify,
        KillPoint::BeforeRollback,
        KillPoint::BeforeResidue,
        KillPoint::AfterRollback,
    ];
    for index in 0..step_count {
        points.push(KillPoint::BeforeStep(index));
        points.push(KillPoint::AfterStep(index));
    }
    points
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn any_injected_failure_leaves_either_a_verified_fix_or_the_exact_pre_state(
        step_count in 1usize..5,
        fail_at in prop::option::of(0usize..5),
        verify_passes in any::<bool>(),
        kill_index in 0usize..64,
    ) {
        let targets: Vec<(String, i128)> = (0..step_count)
            .map(|index| (format!("job:{index}"), i128::try_from(index).unwrap_or(0) * 10 + 1))
            .collect();
        let borrow: Vec<(&str, i128)> = targets
            .iter()
            .map(|(name, value)| (name.as_str(), *value))
            .collect();
        let options = if verify_passes {
            FakeOptions::default()
        } else {
            FakeOptions::symptom_fails()
        };
        let mut harness = Harness::with_options(
            &borrow,
            Tier::T1,
            FixPolicy::default().with_rehearsal_optional(common::REMEDY),
            options,
        );
        harness.exec.fail_at = fail_at.filter(|index| *index < step_count);

        let points = kill_points(step_count);
        let point = points[kill_index % points.len()].clone();
        let fix = common::fix_id();
        let kill = KillAt::new(point.clone());
        let outcome = match harness.run(&fix, &kill) {
            Err(FixError::Killed { .. }) => harness.recover(&fix).unwrap(),
            Ok(outcome) => outcome,
            Err(error) => panic!("unexpected pipeline error at {point:?}: {error}"),
        };

        let pre = harness.pre_state();
        let post = harness.post_state();
        match outcome.state {
            FixState::Verified => {
                prop_assert_eq!(
                    harness.world.snapshot().unwrap(),
                    post,
                    "a verified fix is fully applied at {:?}",
                    point
                );
            }
            FixState::RolledBack => {
                prop_assert_eq!(
                    harness.world.snapshot().unwrap(),
                    pre,
                    "a rollback is byte-identical to the pre-state at {:?}",
                    point
                );
                let residue = outcome.residue.expect("a rollback measured a residue");
                prop_assert!(residue.is_empty(), "no residue may remain at {:?}", point);
            }
            FixState::Proposed | FixState::Failed => {
                prop_assert_eq!(
                    harness.world.snapshot().unwrap(),
                    pre,
                    "nothing was applied at {:?}",
                    point
                );
            }
            other => prop_assert!(false, "unexpected state {other} at {point:?}"),
        }
    }
}
