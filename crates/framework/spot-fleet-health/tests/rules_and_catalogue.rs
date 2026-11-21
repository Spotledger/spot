//! Rule-set loading and the error catalogue: the data files and the codes stay complete.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_fleet_health::{FleetHealthError, RuleId, RuleSet, Signal};

#[test]
fn every_refusal_has_a_catalogue_entry() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(catalogue.load_error(), None, "the catalogue loads cleanly");
    let errors = [
        FleetHealthError::InvalidIdentifier { kind: "tenant" },
        FleetHealthError::InvalidSettings {
            reason: "bad".to_owned(),
        },
        FleetHealthError::InvalidRuleSet {
            reason: "bad".to_owned(),
        },
        FleetHealthError::InvalidObservation {
            reason: "bad".to_owned(),
        },
        FleetHealthError::InvalidSilence {
            reason: "bad".to_owned(),
        },
    ];
    for error in errors {
        assert!(
            catalogue.lookup(error.code()).is_some(),
            "{} is catalogued",
            error.code()
        );
    }
}

#[test]
fn the_builtin_rule_set_covers_every_signal_with_both_colours() {
    let rules = RuleSet::builtin().expect("builtin rules");
    for signal in Signal::ALL {
        let for_signal: Vec<_> = rules
            .rules()
            .iter()
            .filter(|rule| rule.signal == signal)
            .collect();
        assert!(!for_signal.is_empty(), "{signal} has a rule");
    }
    assert!(
        rules
            .rules()
            .iter()
            .any(|rule| rule.severity == spot_fleet_health::Health::Amber)
    );
    assert!(
        rules
            .rules()
            .iter()
            .any(|rule| rule.severity == spot_fleet_health::Health::Red)
    );
}

#[test]
fn a_rule_id_cannot_look_like_an_error_code() {
    assert!(RuleId::try_new("health_probe_down").is_ok());
    assert!(RuleId::try_new("health.probe_down").is_err());
    assert!(RuleId::try_new("Upper").is_err());
    assert!(RuleId::try_new("").is_err());
}

#[test]
fn a_missing_freshness_register_is_refused() {
    let toml = r#"
        [[rule]]
        id = "queue_lag_high"
        signal = "queue_lag"
        subject = "tenant"
        comparison = "above"
        threshold = 60
        for_seconds = 60
        severity = "amber"
        sentence = "Queue lag for {subject} is {value}."
        action = "Wait."
    "#;
    let error = RuleSet::parse(&[toml]).expect_err("missing freshness");
    assert_eq!(error.code(), "fleet_health.invalid_rule_set");
}

#[test]
fn an_unknown_rule_field_is_refused() {
    let toml = r#"
        [[rule]]
        id = "queue_lag_high"
        signal = "queue_lag"
        subject = "tenant"
        comparison = "above"
        threshold = 60
        for_seconds = 60
        severity = "amber"
        sentence = "Queue lag for {subject} is {value}."
        action = "Wait."
        labels = { user = "someone" }

        [freshness]
        after_seconds = 45
        sentence = "No data from {subject} for {duration}."
        no_data_sentence = "No data from {subject}."
        action = "Check it."
    "#;
    let error = RuleSet::parse(&[toml]).expect_err("unknown field");
    assert_eq!(error.code(), "fleet_health.invalid_rule_set");
}
