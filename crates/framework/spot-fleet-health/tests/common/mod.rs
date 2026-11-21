//! Shared fixtures for the fleet health tests.
//!
//! All times are offsets from a fixed base instant, so every test is a simulated clock: no
//! sleep, no wall clock, no flakiness.

#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use spot_fleet_health::{
    FleetHealth, Health, Observation, RuleSet, Signal, Subject, SubjectHealth, TenantId, Timestamp,
    evaluate,
};

/// The base instant every test counts from.
pub const BASE: i64 = 1_700_000_000;

/// The instant `offset` seconds after [`BASE`].
pub fn at(offset: u64) -> Timestamp {
    Timestamp::from_secs(BASE.saturating_add(i64::try_from(offset).expect("offset")))
}

/// A tenant subject.
pub fn tenant(id: &str) -> Subject {
    Subject::Tenant(TenantId::try_new(id).expect("tenant id"))
}

/// The shipped rule set.
pub fn rules() -> RuleSet {
    RuleSet::builtin().expect("builtin rules")
}

/// One observation at `offset` seconds after [`BASE`].
pub fn observation(subject: &Subject, signal: Signal, value: i64, offset: u64) -> Observation {
    Observation::new(subject.clone(), signal, value, at(offset)).expect("observation")
}

/// Evaluates one subject at `offset` with the shipped rules and default settings.
pub fn view_one(subject: &Subject, observations: &[Observation], offset: u64) -> FleetHealth {
    evaluate(
        std::slice::from_ref(subject),
        observations,
        &rules(),
        &spot_fleet_health::Settings::default(),
        at(offset),
    )
    .expect("evaluation")
}

/// A fleet view assembled directly from findings, so alert tests can skip the rule clock.
pub fn view_of(findings: Vec<spot_fleet_health::Finding>) -> FleetHealth {
    let mut by_subject: BTreeMap<Subject, Vec<spot_fleet_health::Finding>> = BTreeMap::new();
    for finding in findings {
        by_subject
            .entry(finding.subject.clone())
            .or_default()
            .push(finding);
    }
    let subjects = by_subject
        .into_iter()
        .map(|(subject, mut findings)| {
            findings.sort_by(|a, b| {
                b.health
                    .cmp(&a.health)
                    .then_with(|| a.since.cmp(&b.since))
                    .then_with(|| a.rule_id.cmp(&b.rule_id))
            });
            let health = findings
                .first()
                .map_or(Health::Green, |finding| finding.health);
            let sentence = findings.first().map_or_else(
                || "Everything is working normally.".to_owned(),
                |f| f.sentence.clone(),
            );
            SubjectHealth {
                subject,
                health,
                sentence,
                action: findings.first().map(|finding| finding.action.clone()),
                findings,
                last_observed_at: None,
                stale: false,
                no_data: false,
            }
        })
        .collect();
    FleetHealth {
        generated_at: at(0),
        subjects,
    }
}

/// One finding for `subject` from `rule`.
pub fn finding(
    subject: &Subject,
    rule: &str,
    health: Health,
    sentence: &str,
) -> spot_fleet_health::Finding {
    spot_fleet_health::Finding {
        subject: subject.clone(),
        rule_id: spot_fleet_health::RuleId::try_new(rule).expect("rule id"),
        signal: None,
        health,
        value: None,
        since: at(0),
        observed_at: at(0),
        sentence: sentence.to_owned(),
        action: "Fix it.".to_owned(),
    }
}
