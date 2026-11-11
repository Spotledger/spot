//! The state machine's contract: a canary failure halts the fleet, no pause resumes itself,
//! criteria and soak gate each wave, and waves overlap only when declared.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_fleet_rollout::{
    Action, FailureKind, Health, Moment, NodeFailure, NodeObservation, Observations, PauseOn,
    PauseReason, Rollout, Seed, State,
};
use support::{canary, fleet, plan, wave};

fn rollout_of(waves: Vec<spot_fleet_rollout::Wave>) -> (Rollout, usize) {
    let fleet = fleet(20);
    let plan = plan(waves);
    let count = plan.waves.len();
    (Rollout::new(plan, &fleet, Seed::new(3)).unwrap(), count)
}

#[test]
fn a_canary_failure_pauses_everything_and_names_the_node() {
    let (mut rollout, _) = rollout_of(vec![
        canary(2, 10),
        wave("middle", 4, 10),
        wave("tail", 20, 0),
    ]);
    let start = rollout.advance(&Observations::new(), Moment::ZERO);
    assert!(matches!(
        start,
        Action::StartWave {
            index: 0,
            canary: true,
            ..
        }
    ));

    let failed = rollout.schedule()[0].nodes[0].clone();
    let action = rollout.advance(
        &Observations::new().fail(NodeFailure::new(
            failed.clone(),
            FailureKind::Migrate,
            "the patch failed",
        )),
        Moment::ZERO,
    );

    let Action::Pause { pause } = action else {
        panic!("a canary failure must pause, got {action:?}");
    };
    assert_eq!(pause.reason, PauseReason::CanaryFailure);
    assert_eq!(pause.node, Some(failed.clone()));
    assert_eq!(pause.wave, Some(0));
    assert!(pause.detail.contains(failed.as_str()));
    assert_eq!(rollout.state(), State::Paused);
    assert_eq!(rollout.failures().len(), 1);
    assert_eq!(rollout.failures()[0].wave, Some(0));

    // Healthy observations afterwards never resume it.
    let later = rollout.advance(
        &Observations::new().observe(NodeObservation::healthy(failed)),
        Moment::from_secs(1_000),
    );
    assert!(matches!(later, Action::Pause { .. }));
}

#[test]
fn a_canary_failure_pauses_even_when_any_failure_is_disabled() {
    let fleet = fleet(10);
    let mut plan = plan(vec![canary(2, 10), wave("tail", 10, 0)]);
    plan.pause_on = PauseOn {
        any_failure: false,
        max_error_rate_permille: 1_000,
        min_health: Health::Red,
    };
    let mut rollout = Rollout::new(plan, &fleet, Seed::new(1)).unwrap();
    rollout.advance(&Observations::new(), Moment::ZERO);
    let failed = rollout.schedule()[0].nodes[1].clone();
    let action = rollout.advance(
        &Observations::new().fail(NodeFailure::new(
            failed,
            FailureKind::Crash,
            "lost the node",
        )),
        Moment::ZERO,
    );
    assert!(matches!(
        action,
        Action::Pause {
            pause: spot_fleet_rollout::PauseRecord {
                reason: PauseReason::CanaryFailure,
                ..
            }
        }
    ));
}

#[test]
fn an_error_rate_breach_pauses_and_names_the_node() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10), wave("tail", 20, 0)]);
    rollout.advance(&Observations::new(), Moment::ZERO);
    let noisy = rollout.schedule()[0].nodes[0].clone();
    let action = rollout.advance(
        &Observations::new().observe(NodeObservation::new(noisy.clone(), Health::Green, 99, true)),
        Moment::ZERO,
    );
    let Action::Pause { pause } = action else {
        panic!("an error-rate breach must pause, got {action:?}");
    };
    assert_eq!(pause.reason, PauseReason::ErrorRate);
    assert_eq!(pause.node, Some(noisy));
}

#[test]
fn a_health_breach_pauses_and_names_the_node() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10), wave("tail", 20, 0)]);
    rollout.advance(&Observations::new(), Moment::ZERO);
    let sick = rollout.schedule()[0].nodes[1].clone();
    let action = rollout.advance(
        &Observations::new()
            .observe(NodeObservation::healthy(sick.clone()).with_health(Health::Red)),
        Moment::ZERO,
    );
    let Action::Pause { pause } = action else {
        panic!("a health breach must pause, got {action:?}");
    };
    assert_eq!(pause.reason, PauseReason::Health);
    assert_eq!(pause.node, Some(sick));
}

#[test]
fn a_verify_failure_pauses_and_names_the_node() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10), wave("tail", 20, 0)]);
    rollout.advance(&Observations::new(), Moment::ZERO);
    let unverified = rollout.schedule()[0].nodes[0].clone();
    let action = rollout.advance(
        &Observations::new().observe(NodeObservation::new(
            unverified.clone(),
            Health::Green,
            0,
            false,
        )),
        Moment::ZERO,
    );
    let Action::Pause { pause } = action else {
        panic!("an unverified node must pause, got {action:?}");
    };
    assert_eq!(pause.reason, PauseReason::VerifyFailed);
    assert_eq!(pause.node, Some(unverified));
}

#[test]
fn a_clean_rollout_soaks_each_wave_and_completes() {
    let (mut rollout, wave_count) = rollout_of(vec![
        canary(2, 10),
        wave("middle", 4, 10),
        wave("tail", 20, 10),
    ]);
    let mut now = Moment::ZERO;
    let mut observations = Observations::new();
    let mut started = 0;
    for _ in 0..64 {
        match rollout.advance(&observations, now) {
            Action::StartWave { index, .. } => {
                started += 1;
                for node in &rollout.schedule()[index].nodes {
                    observations
                        .nodes
                        .push(NodeObservation::healthy(node.clone()));
                }
            }
            Action::Soak { .. } => now = now.saturating_add(10_000),
            Action::Complete { waves } => {
                assert_eq!(waves, wave_count);
                assert_eq!(started, wave_count);
                assert_eq!(rollout.state(), State::Complete);
                return;
            }
            other => panic!("unexpected action {other:?}"),
        }
    }
    panic!("the clean rollout did not complete");
}

#[test]
fn a_pause_currently_never_resumes_by_itself_and_resume_restarts_the_soak() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10), wave("tail", 20, 10)]);
    rollout.advance(&Observations::new(), Moment::ZERO);
    let failed = rollout.schedule()[0].nodes[0].clone();
    rollout.advance(
        &Observations::new().fail(NodeFailure::new(
            failed.clone(),
            FailureKind::Timeout,
            "no answer",
        )),
        Moment::ZERO,
    );
    let clean = Observations {
        nodes: rollout.schedule()[0]
            .nodes
            .iter()
            .cloned()
            .map(NodeObservation::healthy)
            .collect(),
        failures: Vec::new(),
    };
    for second in [1_u64, 10, 100, 1_000] {
        assert!(matches!(
            rollout.advance(&clean, Moment::from_secs(second)),
            Action::Pause { .. }
        ));
    }

    let resume = rollout.resume(Moment::from_secs(50)).unwrap();
    assert!(matches!(resume, Action::Resume { index: 0 }));
    assert_eq!(rollout.resumes(), 1);
    assert_eq!(rollout.pause_log().len(), 1);
    assert_eq!(rollout.state(), State::Rolling);

    // The soak restarted at 50 s: at 55 s it is still soaking.
    assert!(matches!(
        rollout.advance(&clean, Moment::from_secs(55)),
        Action::Soak { .. }
    ));
    // At 60 s the canary passed and the next wave starts.
    assert!(matches!(
        rollout.advance(&clean, Moment::from_secs(60)),
        Action::StartWave { index: 1, .. }
    ));
}

#[test]
fn abort_is_terminal_and_idempotent() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10), wave("tail", 20, 0)]);
    rollout.advance(&Observations::new(), Moment::ZERO);
    let aborted = rollout.abort("operator stopped the rollout").unwrap();
    assert!(matches!(aborted, Action::Abort { .. }));
    assert_eq!(rollout.state(), State::Aborted);
    let again = rollout.abort("ignored").unwrap();
    assert_eq!(
        again,
        Action::Abort {
            reason: "operator stopped the rollout".to_owned()
        }
    );
    assert!(matches!(
        rollout.advance(&Observations::new(), Moment::from_secs(100)),
        Action::Abort { .. }
    ));
}

#[test]
fn waves_do_not_overlap_by_default() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10), wave("middle", 4, 10)]);
    rollout.advance(&Observations::new(), Moment::ZERO);
    let mut observations = Observations::new();
    for node in &rollout.schedule()[0].nodes {
        observations
            .nodes
            .push(NodeObservation::healthy(node.clone()));
    }
    for second in 0..10 {
        let action = rollout.advance(&observations, Moment::from_secs(second));
        assert!(
            matches!(action, Action::Soak { index: 0, .. }),
            "wave 1 must not start before wave 0 soaked, got {action:?}"
        );
    }
    assert!(matches!(
        rollout.advance(&observations, Moment::from_secs(10)),
        Action::StartWave { index: 1, .. }
    ));
}

#[test]
fn a_declared_overlap_may_start_the_next_wave_early() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10), wave("middle", 4, 10).overlapping()]);
    rollout.advance(&Observations::new(), Moment::ZERO);
    let mut observations = Observations::new();
    for node in &rollout.schedule()[0].nodes {
        observations
            .nodes
            .push(NodeObservation::healthy(node.clone()));
    }
    assert!(matches!(
        rollout.advance(&observations, Moment::ZERO),
        Action::StartWave { index: 1, .. }
    ));
}

#[test]
fn an_unreported_node_pauses_once_the_soak_elapsed() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10), wave("tail", 20, 0)]);
    rollout.advance(&Observations::new(), Moment::ZERO);
    assert!(matches!(
        rollout.advance(&Observations::new(), Moment::ZERO),
        Action::Soak { .. }
    ));
    let action = rollout.advance(&Observations::new(), Moment::from_secs(10));
    let Action::Pause { pause } = action else {
        panic!("a missing observation after the soak must pause, got {action:?}");
    };
    assert_eq!(pause.reason, PauseReason::MissingObservation);
}

#[test]
fn resume_on_a_running_rollout_is_refused() {
    let (mut rollout, _) = rollout_of(vec![canary(2, 10)]);
    assert_eq!(
        rollout.resume(Moment::ZERO).unwrap_err().code(),
        "fleet_rollout.not_paused"
    );
}
