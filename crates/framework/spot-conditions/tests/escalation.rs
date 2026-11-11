//! Escalation by elapsed time (ADR-067 section 1): each rise is one audited transition, and the
//! schedule never lowers a condition.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    AuditEvent, ConditionDef, ConditionEngine, ConditionParams, ConditionTopic, Escalation,
    MemorySink, Millis, Registry, Severity, Transition,
};

const WINDOW: u64 = 60_000;

fn ms(millis: u64) -> Millis {
    Millis::new(millis)
}

fn engine_with(def: ConditionDef) -> ConditionEngine<MemorySink> {
    let mut registry = Registry::new();
    registry.register(def).unwrap();
    ConditionEngine::new(registry, MemorySink::new())
}

/// `NODE_SILENT`: amber, red after an hour.
fn node_silent() -> ConditionEngine<MemorySink> {
    engine_with(
        ConditionDef::new(
            "NODE_SILENT",
            Severity::Amber,
            "node",
            "condition.node_silent",
        )
        .with_param("silent_hours")
        .with_escalation(Escalation::new(ms(3_600_000), Severity::Red)),
    )
}

#[test]
fn the_severity_rises_when_the_step_comes_due_and_not_a_millisecond_before() {
    let mut engine = node_silent();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    assert_eq!(engine.open()[0].severity, Severity::Amber);

    assert_eq!(engine.tick(ms(3_599_999)).escalated, 0);
    assert_eq!(engine.open()[0].severity, Severity::Amber);
    assert_eq!(engine.sink().len(), 1);

    assert_eq!(engine.tick(ms(3_600_000)).escalated, 1);
    assert_eq!(engine.open()[0].severity, Severity::Red);
    let events = engine.sink().events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].transition(), Transition::Escalated);
    assert_eq!(events[1].severity(), Severity::Red);
    assert_eq!(events[1].at_millis(), ms(3_600_000));
    match &events[1] {
        AuditEvent::Escalated { from, to, .. } => {
            assert_eq!(*from, Severity::Amber);
            assert_eq!(*to, Severity::Red);
        }
        other => panic!("expected an escalated event, got {other:?}"),
    }
}

#[test]
fn a_tick_with_nothing_due_and_a_repeated_tick_change_nothing() {
    let mut engine = node_silent();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    assert_eq!(engine.tick(ms(0)), Default::default());
    engine.tick(ms(3_600_000));
    let after_first_rise = engine.sink().len();
    assert_eq!(engine.tick(ms(3_600_000)).escalated, 0);
    assert_eq!(engine.tick(ms(90_000_000)).escalated, 0);
    assert_eq!(engine.sink().len(), after_first_rise);
    assert_eq!(engine.open()[0].severity, Severity::Red);
}

#[test]
fn every_due_step_is_one_event_in_schedule_order() {
    let mut engine = engine_with(
        ConditionDef::new(
            "BOOTSTRAP_STALLED",
            Severity::Info,
            "node",
            "condition.stalled",
        )
        .with_escalation(Escalation::new(ms(20_000), Severity::Red))
        .with_escalation(Escalation::new(ms(10_000), Severity::Amber)),
    );
    engine
        .raise(
            "BOOTSTRAP_STALLED",
            "branch-4",
            ConditionParams::new(),
            ms(0),
        )
        .unwrap();

    // One tick after both steps are due: two rises, each its own event, in elapsed-time order
    // even though the definition listed them the other way round.
    assert_eq!(engine.tick(ms(30_000)).escalated, 2);
    let events = engine.sink().events();
    assert_eq!(events.len(), 3);
    match (&events[1], &events[2]) {
        (
            AuditEvent::Escalated {
                from: f1, to: t1, ..
            },
            AuditEvent::Escalated {
                from: f2, to: t2, ..
            },
        ) => {
            assert_eq!((*f1, *t1), (Severity::Info, Severity::Amber));
            assert_eq!((*f2, *t2), (Severity::Amber, Severity::Red));
        }
        other => panic!("expected two escalated events, got {other:?}"),
    }
    assert_eq!(engine.open()[0].severity, Severity::Red);
}

#[test]
fn a_step_whose_target_is_not_higher_never_lowers_the_condition() {
    let mut engine = engine_with(
        ConditionDef::new(
            "QUARANTINED",
            Severity::Red,
            "record",
            "condition.quarantined",
        )
        .with_escalation(Escalation::new(ms(1_000), Severity::Amber))
        .with_escalation(Escalation::new(ms(2_000), Severity::Red)),
    );
    engine
        .raise("QUARANTINED", "record-1", ConditionParams::new(), ms(0))
        .unwrap();
    assert_eq!(engine.tick(ms(10_000)).escalated, 0);
    assert_eq!(engine.open()[0].severity, Severity::Red);
    assert_eq!(engine.sink().len(), 1, "no transition happened");
}

#[test]
fn an_escalation_is_published_on_the_raised_topic() {
    let mut engine = node_silent();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine.tick(ms(3_600_000));
    let event = &engine.sink().events()[1];
    assert_eq!(event.transition(), Transition::Escalated);
    assert_eq!(
        event.topic(),
        ConditionTopic::Raised,
        "ADR-061 has two condition topics: a rise is a raise at a higher severity"
    );
    assert_eq!(event.topic().as_str(), "sys_condition_raised");
    assert_eq!(
        engine.history("branch-12")[1].transition.as_str(),
        "escalated"
    );
}

#[test]
fn a_debounced_resurrection_keeps_the_original_schedule() {
    let mut engine = node_silent();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(50_000))
        .unwrap();
    // Back inside the window: the same condition, so the hour still counts from the first raise.
    assert_eq!(
        engine
            .raise(
                "NODE_SILENT",
                "branch-12",
                ConditionParams::new(),
                ms(60_000)
            )
            .unwrap(),
        spot_conditions::RaiseOutcome::Debounced
    );
    assert_eq!(engine.open()[0].severity, Severity::Amber);

    assert_eq!(engine.tick(ms(3_600_000)).escalated, 1);
    assert_eq!(engine.open()[0].severity, Severity::Red);
    assert_eq!(engine.sink().len(), 2);
}

#[test]
fn a_condition_raised_again_after_the_window_starts_its_schedule_from_the_new_raise() {
    let mut engine = node_silent();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(10_000))
        .unwrap();
    engine.tick(ms(10_000 + WINDOW));
    engine
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            ms(3_600_000),
        )
        .unwrap();

    // The new condition starts at the definition's severity, and its hour starts at 3_600_000.
    assert_eq!(engine.open()[0].severity, Severity::Amber);
    assert_eq!(engine.tick(ms(3_600_000 + 3_599_999)).escalated, 0);
    assert_eq!(engine.tick(ms(3_600_000 + 3_600_000)).escalated, 1);
}

#[test]
fn an_escalation_event_carries_the_parameters_of_its_condition() {
    let mut engine = node_silent();
    engine
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new().with("silent_hours", "31"),
            ms(0),
        )
        .unwrap();
    engine.tick(ms(3_600_000));
    let events = engine.sink().events();
    assert_eq!(events[1].params().get("silent_hours"), Some("31"));
}
