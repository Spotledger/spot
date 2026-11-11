//! The 500-node fleet simulator: a canary failure halts every later wave, a failure never
//! starts anything after its wave, and the skew rule admits no unverified or over-skewed node.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_fleet_rollout::{PauseReason, SimConfig, SimReport, SkewLimit, run, run_seeds};

fn canary_config(seed: u64) -> SimConfig {
    SimConfig {
        seed: spot_fleet_rollout::Seed::new(seed),
        failure_wave: Some(0),
        ..SimConfig::default()
    }
}

#[test]
fn a_canary_failure_halts_the_fleet_for_every_seed() {
    let reports = run_seeds(&canary_config(0), &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    assert_eq!(reports.len(), 8);
    for report in reports {
        assert_eq!(report.waves_started, vec![0], "no later wave may start");
        assert!(!report.completed);
        let pause = report.pause.as_ref().unwrap();
        assert_eq!(pause.reason, PauseReason::CanaryFailure);
        assert_eq!(pause.wave, Some(0));
        assert_eq!(
            pause.node,
            report.failure.as_ref().map(|failure| failure.node.clone())
        );
        assert_eq!(
            report.refused_unverified,
            report.considered - report.admitted - report.refused_skew
        );
        assert!(report.refused_unverified > 0, "later nodes never verified");
    }
}

#[test]
fn an_injected_failure_at_a_random_wave_never_starts_a_later_wave() {
    let config = SimConfig {
        seed: spot_fleet_rollout::Seed::new(0),
        ..SimConfig::default()
    };
    let reports = run_seeds(&config, &(0..16).collect::<Vec<_>>()).unwrap();
    let mut saw_early = false;
    let mut saw_late = false;
    for report in reports {
        let failure = report.failure.as_ref().unwrap();
        assert!(
            report
                .waves_started
                .iter()
                .all(|started| *started <= failure.wave),
            "started {:?} after failure in wave {}",
            report.waves_started,
            failure.wave
        );
        let pause = report.pause.as_ref().unwrap();
        assert_eq!(pause.node.as_ref(), Some(&failure.node));
        assert_eq!(pause.wave, Some(failure.wave));
        if failure.wave == 0 {
            saw_early = true;
        } else {
            saw_late = true;
        }
    }
    assert!(saw_early && saw_late, "seeds cover more than one wave");
}

#[test]
fn a_clean_run_completes_and_admits_only_verified_within_skew_nodes() {
    let report = run(SimConfig {
        inject_failure: false,
        ..SimConfig::default()
    })
    .unwrap();
    assert!(report.completed);
    assert!(report.pause.is_none());
    assert_eq!(report.waves_started, vec![0, 1, 2, 3]);
    assert_eq!(
        report.considered, 503,
        "500 fleet nodes and 3 offline nodes"
    );
    assert_eq!(report.admitted, 500);
    assert_eq!(report.refused_unverified, 0);
    assert_eq!(
        report.refused_skew, 3,
        "the offline nodes are two epochs behind"
    );
}

#[test]
fn a_wider_skew_limit_admits_the_lagging_nodes() {
    let report = run(SimConfig {
        inject_failure: false,
        epoch: spot_fleet_rollout::Epoch::new(5),
        skew_limit: SkewLimit::new(2),
        ..SimConfig::default()
    })
    .unwrap();
    assert!(report.completed);
    assert_eq!(report.admitted, 503);
    assert_eq!(report.refused_skew, 0);
    assert_eq!(report.refused_unverified, 0);
}

#[test]
fn the_run_is_deterministic() {
    let config = SimConfig::default();
    let first: SimReport = run(config.clone()).unwrap();
    let second = run(config).unwrap();
    assert_eq!(first, second);
}
