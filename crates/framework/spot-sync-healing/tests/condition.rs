//! The `RECONCILE_DIVERGENCE` definition this crate owns. Test code may unwrap.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_conditions::{
    ConditionEngine, ConditionParams, ConditionsError, MemorySink, Registry, Severity,
};
use spot_sync_healing::{
    CONDITION_ID, Cause, ConditionSignal, HealingDriver, HealingSettings, MemoryJournal,
    SUBJECT_KIND, apply_signals, condition_def, register_conditions,
};

fn gave_up() -> Vec<ConditionSignal> {
    let mut d = HealingDriver::new(
        Script::failing("down"),
        MemoryJournal::new(),
        HealingSettings::default(),
    )
    .unwrap();
    let o = obs("branch", vec![missing("A")]);
    for t in [0, 10_000, 30_000, 70_000] {
        d.step(std::slice::from_ref(&o), ms(t));
    }
    d.take_signals()
}

#[test]
fn the_definition_has_the_agreed_shape() {
    let def = condition_def();
    assert_eq!(def.id, CONDITION_ID);
    assert_eq!(def.severity, Severity::Amber);
    assert_eq!(def.subject_kind, SUBJECT_KIND);
    assert_eq!(def.params, ["node", "scope", "cause", "attempts"]);
    assert!(def.response.is_empty());
    assert_eq!(def.actions, ["re_verify"]);
    assert_eq!(
        def.explanation_key,
        "sync.condition.reconcile_divergence.explanation"
    );
    assert_eq!(SUBJECT_KIND, "node_scope");
}

#[test]
fn a_give_up_raise_carries_every_declared_parameter() {
    let signals = gave_up();
    let [ConditionSignal::Raise { params, .. }] = &signals[..] else {
        panic!("expected one raise");
    };
    assert!(condition_def().missing_params(params).is_empty());
}

#[test]
fn registering_twice_is_a_typed_duplicate_error() {
    let mut registry = Registry::new();
    register_conditions(&mut registry).unwrap();
    assert!(matches!(
        register_conditions(&mut registry),
        Err(ConditionsError::DuplicateId { .. })
    ));
}

#[test]
fn an_unregistered_engine_fails_loud_and_typed() {
    let mut engine = ConditionEngine::new(Registry::new(), MemorySink::new());
    let result = apply_signals(&mut engine, &gave_up(), ms(70_000));
    assert!(matches!(result, Err(ConditionsError::UnknownId { .. })));
}

#[test]
fn every_cause_string_is_accepted_as_the_cause_value() {
    let causes = [
        Cause::AttemptsExhausted,
        Cause::Refused("sync_healing.direction_ambiguous"),
        Cause::Refused("sync_healing.mirror_ahead"),
    ];
    let mut registry = Registry::new();
    register_conditions(&mut registry).unwrap();
    let mut engine = ConditionEngine::new(registry, MemorySink::new());
    for (i, cause) in causes.iter().enumerate() {
        let params = ConditionParams::new()
            .with("node", "n")
            .with("scope", "s")
            .with("cause", cause.as_str())
            .with("attempts", "4");
        assert!(condition_def().missing_params(&params).is_empty());
        let signal = ConditionSignal::Raise {
            subject: format!("n{i}/s"),
            params,
            cause: cause.clone(),
            evidence: vec![],
        };
        apply_signals(&mut engine, &[signal], ms(0)).unwrap();
    }
    assert_eq!(engine.open().len(), causes.len());
}
