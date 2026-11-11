//! The pure evaluation: rules over a window of observations, at an explicit `now`.
//!
//! [`evaluate`] takes the subjects the caller knows about, the observations collected for
//! them, the rule set, the settings and the instant to evaluate for. It reads nothing else,
//! writes nothing, and returns a [`FleetHealth`] the console or an alert manager can use.
//!
//! # The window and "sustained"
//!
//! Only observations at or after `now - window_seconds` and at or before `now` are considered.
//! A rule fires when the **first** breaching reading in the window is at least `for_seconds`
//! old and every later reading for that signal is also a breach. A later green reading clears
//! the finding, which is what makes recovery immediate in the view and lets the alert manager
//! apply its own hysteresis on top.
//!
//! # Freshness
//!
//! A subject with no reading inside the freshness threshold gets an amber finding from the
//! `[freshness]` data entry. A subject that has never reported gets the no-data sentence. This
//! is how "a silent node is not green" and "never report green for a tenant with no recent
//! data" are enforced in one place.

use std::collections::BTreeSet;

use crate::error::FleetHealthError;
use crate::health::{Finding, FleetHealth, Health, SubjectHealth};
use crate::rules::{Rule, RuleId, RuleSet};
use crate::settings::Settings;
use crate::signal::{Observation, Signal, Subject};
use crate::time::{Timestamp, humanise_seconds};

/// Evaluates the fleet at `now`.
///
/// `subjects` is the set of tenants and nodes the caller wants a status for. A subject with no
/// observations still gets an entry, amber by the freshness register, so a tenant cannot be
/// silently missing from the view.
///
/// # Errors
///
/// [`FleetHealthError::InvalidSettings`] when the cadence or window is inconsistent.
pub fn evaluate(
    subjects: &[Subject],
    observations: &[Observation],
    rules: &RuleSet,
    settings: &Settings,
    now: Timestamp,
) -> Result<FleetHealth, FleetHealthError> {
    settings.validate()?;
    let unique: BTreeSet<Subject> = subjects.iter().cloned().collect();
    let mut entries = Vec::with_capacity(unique.len());
    for subject in unique {
        entries.push(evaluate_subject(
            &subject,
            observations,
            rules,
            settings,
            now,
        ));
    }
    Ok(FleetHealth {
        generated_at: now,
        subjects: entries,
    })
}

fn evaluate_subject(
    subject: &Subject,
    observations: &[Observation],
    rules: &RuleSet,
    settings: &Settings,
    now: Timestamp,
) -> SubjectHealth {
    let mut mine: Vec<&Observation> = observations
        .iter()
        .filter(|observation| observation.subject == *subject && observation.observed_at <= now)
        .collect();
    mine.sort_by(|a, b| {
        a.observed_at
            .cmp(&b.observed_at)
            .then_with(|| a.signal.cmp(&b.signal))
    });

    let last_observed_at = mine.last().map(|observation| observation.observed_at);
    let window_start = now.saturating_sub_secs(settings.window_seconds);
    let in_window: Vec<&Observation> = mine
        .iter()
        .copied()
        .filter(|observation| observation.observed_at >= window_start)
        .collect();

    let mut findings: Vec<Finding> = Vec::new();
    for rule in rules.rules() {
        if !rule.applies_to(subject) {
            continue;
        }
        if let Some(finding) = evaluate_rule(subject, rule, &in_window, now) {
            findings.push(finding);
        }
    }

    // A red rule for a signal makes the amber rule for the same signal redundant: the view
    // should say the worst thing once, not list a lesser version of it beside it.
    let red_signals: BTreeSet<Signal> = findings
        .iter()
        .filter(|finding| finding.health == Health::Red)
        .filter_map(|finding| finding.signal)
        .collect();
    findings.retain(|finding| match finding.signal {
        Some(signal) => finding.health == Health::Red || !red_signals.contains(&signal),
        None => true,
    });

    let stale = match last_observed_at {
        None => true,
        Some(last) => last.age(now) > rules.freshness.after_seconds,
    };
    let no_data = last_observed_at.is_none();
    if stale {
        let duration = last_observed_at.map(|last| last.age(now));
        let sentence = if no_data {
            render_sentence(&rules.freshness.no_data_sentence, subject, None, None)
        } else {
            render_sentence(&rules.freshness.sentence, subject, None, duration)
        };
        findings.push(Finding {
            subject: subject.clone(),
            rule_id: RuleId::freshness(),
            signal: None,
            health: Health::Amber,
            value: duration.map(|seconds| i64::try_from(seconds).unwrap_or(i64::MAX)),
            since: last_observed_at.unwrap_or(now),
            observed_at: last_observed_at.unwrap_or(now),
            sentence,
            action: rules.freshness.action.clone(),
        });
    }

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
        || format!("Everything is working normally for {}.", subject.display()),
        |finding| finding.sentence.clone(),
    );
    let action = findings
        .first()
        .filter(|finding| finding.health.needs_attention())
        .map(|finding| finding.action.clone());

    SubjectHealth {
        subject: subject.clone(),
        health,
        sentence,
        action,
        findings,
        last_observed_at,
        stale,
        no_data,
    }
}

fn evaluate_rule(
    subject: &Subject,
    rule: &Rule,
    in_window: &[&Observation],
    now: Timestamp,
) -> Option<Finding> {
    let relevant: Vec<&Observation> = in_window
        .iter()
        .copied()
        .filter(|observation| observation.signal == rule.signal)
        .collect();
    // The finding concerns the run of breaching readings that ends at the latest reading. An
    // older, cleared blip earlier in the window must not hide a failure that is sustained now.
    let latest = relevant.last()?;
    if !rule.breaches(latest.value) {
        return None;
    }
    let run = relevant
        .iter()
        .rev()
        .take_while(|observation| rule.breaches(observation.value))
        .count();
    let first = relevant.get(relevant.len().checked_sub(run)?)?;
    if first.observed_at.age(now) < rule.for_seconds {
        return None;
    }
    Some(Finding {
        subject: subject.clone(),
        rule_id: rule.id.clone(),
        signal: Some(rule.signal),
        health: rule.severity,
        value: Some(latest.value),
        since: first.observed_at,
        observed_at: latest.observed_at,
        sentence: render_sentence(
            &rule.sentence,
            subject,
            Some((rule.signal, latest.value)),
            Some(first.observed_at.age(now)),
        ),
        action: rule.action.clone(),
    })
}

fn render_sentence(
    template: &str,
    subject: &Subject,
    value: Option<(Signal, i64)>,
    duration: Option<u64>,
) -> String {
    let mut sentence = template.replace("{subject}", &subject.display());
    if let Some((signal, raw)) = value {
        sentence = sentence.replace("{value}", &signal.format_value(raw));
        sentence = sentence.replace("{signal}", signal.label());
    }
    if let Some(seconds) = duration {
        sentence = sentence.replace("{duration}", &humanise_seconds(seconds));
    }
    sentence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::RuleSet;
    use crate::signal::{NodeId, Observation, TenantId};

    fn subject(id: &str) -> Subject {
        Subject::Tenant(TenantId::try_new(id).expect("tenant id"))
    }

    fn rules() -> RuleSet {
        RuleSet::builtin().expect("builtin rules")
    }

    #[test]
    fn an_empty_subject_list_is_an_empty_green_fleet() {
        let view = evaluate(
            &[],
            &[],
            &rules(),
            &Settings::default(),
            Timestamp::from_secs(1_000),
        )
        .expect("evaluation");
        assert_eq!(view.worst(), Health::Green);
        assert_eq!(view.counts().total(), 0);
    }

    #[test]
    fn a_subject_with_no_data_is_amber() {
        let tenant = subject("acme");
        let view = evaluate(
            std::slice::from_ref(&tenant),
            &[],
            &rules(),
            &Settings::default(),
            Timestamp::from_secs(1_000),
        )
        .expect("evaluation");
        let health = view.get(&tenant).expect("subject");
        assert_eq!(health.health, Health::Amber);
        assert!(health.no_data);
        assert!(health.stale);
        assert!(health.sentence.contains("tenant acme"));
    }

    #[test]
    fn a_tenant_and_a_node_are_evaluated_separately() {
        let tenant = subject("acme");
        let node = Subject::Node(NodeId::try_new("ln-1").expect("node id"));
        let at = Timestamp::from_secs(1_000);
        let observations = vec![
            Observation::new(tenant.clone(), Signal::HealthProbe, 1, at).expect("probe"),
            Observation::new(node.clone(), Signal::HealthProbe, 1, at).expect("probe"),
        ];
        let view = evaluate(
            &[tenant.clone(), node.clone()],
            &observations,
            &rules(),
            &Settings::default(),
            at,
        )
        .expect("evaluation");
        assert_eq!(view.get(&tenant).expect("tenant").health, Health::Green);
        assert_eq!(view.get(&node).expect("node").health, Health::Green);
    }
}
