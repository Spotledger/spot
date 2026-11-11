//! The acceptance path: a seeded failure becomes visible inside one minute, and a silent node
//! turns amber rather than staying green.
//!
//! The clock is simulated. The evaluator is run at the default cadence from the moment the
//! failure is injected, exactly as the control plane would run it.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{observation, tenant};
use spot_fleet_health::{Health, NodeId, Settings, Signal, Subject, evaluate};

#[test]
fn a_seeded_failure_is_red_in_the_fleet_view_within_sixty_seconds() {
    let acme = tenant("acme");
    let settings = Settings::default();
    let injected_at = 10_u64;

    let mut observations = vec![observation(&acme, Signal::HealthProbe, 1, 0)];
    observations.push(observation(&acme, Signal::HealthProbe, 0, injected_at));

    let mut visible_at = None;
    for tick in 0..=4_u64 {
        let now = injected_at + tick * settings.evaluation_interval_seconds;
        let view = evaluate(
            std::slice::from_ref(&acme),
            &observations,
            &common::rules(),
            &settings,
            common::at(now),
        )
        .expect("evaluation");
        if view.get(&acme).expect("tenant").health == Health::Red {
            visible_at = Some(now);
            break;
        }
    }

    let visible_at = visible_at.expect("the seeded failure became red");
    assert!(
        visible_at - injected_at <= 60,
        "visible after {} seconds",
        visible_at - injected_at
    );
    assert_eq!(
        visible_at - injected_at,
        30,
        "the probe rule fires after 30s"
    );
}

#[test]
fn a_silent_node_is_amber_within_one_minute_and_never_green() {
    let node = Subject::Node(NodeId::try_new("ln-1").expect("node"));
    let observations = vec![observation(&node, Signal::HealthProbe, 1, 0)];
    let settings = Settings::default();

    for offset in [46_u64, 60] {
        let view = evaluate(
            std::slice::from_ref(&node),
            &observations,
            &common::rules(),
            &settings,
            common::at(offset),
        )
        .expect("evaluation");
        let health = view.get(&node).expect("node");
        assert_eq!(health.health, Health::Amber, "at {offset}s");
        assert!(health.stale);
    }
}

#[test]
fn the_default_cadence_evaluates_at_least_four_times_inside_a_minute() {
    let settings = Settings::default();
    assert!(settings.evaluation_interval_seconds > 0);
    assert!(60 / settings.evaluation_interval_seconds >= 4);
    assert!(settings.window_seconds >= 60);
}
