//! Rule evaluation: the three colours, sustained breaches, recovery and freshness.
//!
//! Every instant is simulated; the tests assert the behaviour an operator sees, not internal
//! bookkeeping.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{observation, tenant, view_one};
use spot_fleet_health::{Health, Signal};

#[test]
fn healthy_recent_telemetry_is_green_with_a_sentence() {
    let acme = tenant("acme");
    let observations = vec![observation(&acme, Signal::HealthProbe, 1, 0)];
    let view = view_one(&acme, &observations, 0);
    let health = view.get(&acme).expect("tenant");
    assert_eq!(health.health, Health::Green);
    assert!(health.findings.is_empty());
    assert_eq!(
        health.sentence,
        "Everything is working normally for tenant acme."
    );
    assert!(health.action.is_none());
    assert!(!health.stale);
}

#[test]
fn a_subject_with_no_telemetry_is_amber_not_green() {
    let acme = tenant("acme");
    let view = view_one(&acme, &[], 0);
    let health = view.get(&acme).expect("tenant");
    assert_eq!(health.health, Health::Amber);
    assert!(health.no_data);
    assert!(
        health
            .sentence
            .contains("No telemetry has arrived from tenant acme")
    );
    assert!(health.action.is_some());
}

#[test]
fn telemetry_at_the_freshness_edge_is_still_fresh_and_older_is_amber() {
    let acme = tenant("acme");
    let observations = vec![observation(&acme, Signal::HealthProbe, 1, 0)];

    let edge = view_one(&acme, &observations, 45);
    assert_eq!(edge.get(&acme).expect("tenant").health, Health::Green);
    assert!(!edge.get(&acme).expect("tenant").stale);

    let stale = view_one(&acme, &observations, 46);
    let health = stale.get(&acme).expect("tenant");
    assert_eq!(health.health, Health::Amber);
    assert!(health.stale);
    assert!(!health.no_data);
    assert!(health.sentence.contains("for 46 seconds"));
}

#[test]
fn a_breach_must_last_for_the_rule_duration() {
    let acme = tenant("acme");
    let observations = vec![
        observation(&acme, Signal::HealthProbe, 1, 0),
        observation(&acme, Signal::HealthProbe, 0, 10),
    ];

    let before = view_one(&acme, &observations, 39);
    assert_eq!(before.get(&acme).expect("tenant").health, Health::Green);

    let after = view_one(&acme, &observations, 40);
    let health = after.get(&acme).expect("tenant");
    assert_eq!(health.health, Health::Red);
    assert!(health.sentence.contains("has been failing for 30 seconds"));
}

#[test]
fn a_green_reading_clears_the_finding() {
    let acme = tenant("acme");
    let failing = vec![
        observation(&acme, Signal::HealthProbe, 1, 0),
        observation(&acme, Signal::HealthProbe, 0, 10),
        observation(&acme, Signal::HealthProbe, 0, 20),
    ];
    assert_eq!(
        view_one(&acme, &failing, 40)
            .get(&acme)
            .expect("tenant")
            .health,
        Health::Red
    );

    let recovered = vec![
        observation(&acme, Signal::HealthProbe, 1, 0),
        observation(&acme, Signal::HealthProbe, 0, 10),
        observation(&acme, Signal::HealthProbe, 0, 20),
        observation(&acme, Signal::HealthProbe, 1, 30),
    ];
    let health = view_one(&acme, &recovered, 30);
    assert_eq!(health.get(&acme).expect("tenant").health, Health::Green);
}

#[test]
fn a_critical_rule_suppresses_the_amber_rule_for_the_same_signal() {
    let acme = tenant("acme");
    let observations = vec![
        observation(&acme, Signal::ErrorRate, 2_500, 0),
        observation(&acme, Signal::ErrorRate, 2_500, 120),
    ];
    let view = view_one(&acme, &observations, 120);
    let health = view.get(&acme).expect("tenant");
    assert_eq!(health.health, Health::Red);
    let error_rate_findings = health
        .findings
        .iter()
        .filter(|finding| finding.signal == Some(Signal::ErrorRate))
        .count();
    assert_eq!(error_rate_findings, 1, "only the red finding is listed");
    assert!(health.sentence.contains("25%"));
}

#[test]
fn a_licence_in_grace_is_amber_and_an_expired_one_is_red() {
    let acme = tenant("acme");
    let grace = vec![spot_fleet_health::Observation::licence(
        acme.clone(),
        spot_fleet_health::LicenceState::Grace,
        common::at(0),
    )];
    assert_eq!(
        view_one(&acme, &grace, 0)
            .get(&acme)
            .expect("tenant")
            .health,
        Health::Amber
    );

    let expired = vec![spot_fleet_health::Observation::licence(
        acme.clone(),
        spot_fleet_health::LicenceState::Expired,
        common::at(0),
    )];
    let health = view_one(&acme, &expired, 0);
    assert_eq!(health.get(&acme).expect("tenant").health, Health::Red);
    assert!(
        health
            .get(&acme)
            .expect("tenant")
            .findings
            .iter()
            .all(|finding| finding.health == Health::Red)
    );
}

#[test]
fn a_node_rule_only_applies_to_nodes_and_a_tenant_rule_only_to_tenants() {
    let acme = tenant("acme");
    let observations = vec![observation(&acme, Signal::ReplicationLag, 300, 0)];
    let tenant_view = view_one(&acme, &observations, 0);
    assert!(
        tenant_view
            .get(&acme)
            .expect("tenant")
            .findings
            .iter()
            .all(|finding| finding.signal != Some(Signal::ReplicationLag)),
        "a node-only rule does not fire for a tenant"
    );

    let node =
        spot_fleet_health::Subject::Node(spot_fleet_health::NodeId::try_new("ln-1").expect("node"));
    let node_observations = vec![observation(&node, Signal::ReplicationLag, 300, 0)];
    let node_view = view_one(&node, &node_observations, 60);
    assert_eq!(node_view.get(&node).expect("node").health, Health::Red);
    assert!(
        node_view
            .get(&node)
            .expect("node")
            .sentence
            .contains("5 minutes")
    );
}

#[test]
fn future_readings_are_ignored() {
    let acme = tenant("acme");
    let observations = vec![observation(&acme, Signal::HealthProbe, 1, 60)];
    let view = view_one(&acme, &observations, 0);
    let health = view.get(&acme).expect("tenant");
    assert!(health.no_data);
    assert_eq!(health.health, Health::Amber);
}

#[test]
fn the_worst_subject_drives_the_fleet_colour_and_the_attention_list() {
    let acme = tenant("acme");
    let beta = tenant("beta");
    let observations = vec![
        observation(&acme, Signal::HealthProbe, 1, 60),
        observation(&beta, Signal::ErrorRate, 2_500, 0),
        observation(&beta, Signal::ErrorRate, 2_500, 60),
        observation(&beta, Signal::HealthProbe, 1, 60),
    ];
    let view = spot_fleet_health::evaluate(
        &[acme.clone(), beta.clone()],
        &observations,
        &common::rules(),
        &spot_fleet_health::Settings::default(),
        common::at(60),
    )
    .expect("evaluation");
    assert_eq!(view.worst(), Health::Red);
    assert_eq!(view.counts().red, 1);
    assert_eq!(view.counts().green, 1);
    assert_eq!(view.needs_attention().len(), 1);
    assert_eq!(view.needs_attention()[0].subject, beta);
}

#[test]
fn an_earlier_cleared_blip_does_not_hide_a_failure_that_is_sustained_now() {
    let acme = tenant("acme");
    let observations = vec![
        observation(&acme, Signal::HealthProbe, 0, 0),
        observation(&acme, Signal::HealthProbe, 1, 10),
        observation(&acme, Signal::HealthProbe, 0, 20),
        observation(&acme, Signal::HealthProbe, 0, 30),
    ];

    let early = view_one(&acme, &observations, 40);
    assert_eq!(early.get(&acme).expect("tenant").health, Health::Green);

    let sustained = view_one(&acme, &observations, 50);
    let health = sustained.get(&acme).expect("tenant");
    assert_eq!(health.health, Health::Red);
    assert!(health.sentence.contains("has been failing for 30 seconds"));
}
