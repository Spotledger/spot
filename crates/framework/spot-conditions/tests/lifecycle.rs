//! The raise/clear lifecycle, de-duplication per subject and the engine's view of both.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    AuditEvent, ClearOutcome, ConditionDef, ConditionEngine, ConditionParams, ConditionsError,
    MemorySink, Millis, RaiseOutcome, Registry, Severity,
};

const DEBOUNCE: u64 = 60_000;

fn ms(millis: u64) -> Millis {
    Millis::new(millis)
}

fn registry() -> Registry {
    let mut registry = Registry::new();
    registry
        .register(
            ConditionDef::new(
                "NODE_SILENT",
                Severity::Amber,
                "node",
                "condition.node_silent",
            )
            .with_param("silent_hours"),
        )
        .unwrap();
    registry
        .register(ConditionDef::new(
            "JOURNAL_GAP",
            Severity::Red,
            "stream",
            "condition.journal_gap",
        ))
        .unwrap();
    registry
}

fn engine() -> ConditionEngine<MemorySink> {
    ConditionEngine::new(registry(), MemorySink::new())
}

fn params(value: &str) -> ConditionParams {
    ConditionParams::new().with("silent_hours", value)
}

#[test]
fn a_raise_opens_one_condition_and_records_one_event() {
    let mut engine = engine();
    assert_eq!(
        engine
            .raise("NODE_SILENT", "branch-12", params("31"), ms(1_000))
            .unwrap(),
        RaiseOutcome::Raised
    );

    let open = engine.open();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id.as_str(), "NODE_SILENT");
    assert_eq!(open[0].subject.as_str(), "branch-12");
    assert_eq!(open[0].severity, Severity::Amber);
    assert_eq!(open[0].params.get("silent_hours"), Some("31"));
    assert_eq!(open[0].raised_at_millis, ms(1_000));
    assert_eq!(open[0].last_seen_millis, ms(1_000));

    let events = engine.sink().events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].transition().as_str(), "raised");
    assert_eq!(events[0].at_millis(), ms(1_000));
    assert_eq!(events[0].severity(), Severity::Amber);
    assert_eq!(events[0].params().get("silent_hours"), Some("31"));
}

#[test]
fn raising_an_open_condition_updates_it_without_a_new_event() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", params("31"), ms(1_000))
        .unwrap();
    assert_eq!(
        engine
            .raise("NODE_SILENT", "branch-12", params("32"), ms(9_000))
            .unwrap(),
        RaiseOutcome::Updated
    );

    let open = engine.open();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].params.get("silent_hours"), Some("32"));
    assert_eq!(open[0].raised_at_millis, ms(1_000));
    assert_eq!(open[0].last_seen_millis, ms(9_000));
    assert_eq!(engine.sink().len(), 1, "de-duplication records nothing");
}

#[test]
fn raising_again_with_an_earlier_instant_does_not_move_last_seen_backwards() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", params("31"), ms(10_000))
        .unwrap();
    engine
        .raise("NODE_SILENT", "branch-12", params("31"), ms(4_000))
        .unwrap();
    assert_eq!(engine.open()[0].last_seen_millis, ms(10_000));
}

#[test]
fn conditions_are_de_duplicated_per_subject_not_per_id() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", params("31"), ms(0))
        .unwrap();
    engine
        .raise("NODE_SILENT", "branch-13", params("4"), ms(0))
        .unwrap();
    let open = engine.open();
    assert_eq!(open.len(), 2);
    assert_eq!(open[0].subject.as_str(), "branch-12");
    assert_eq!(open[1].subject.as_str(), "branch-13");
    assert_eq!(engine.sink().len(), 2);
}

#[test]
fn an_observed_clear_is_pending_until_the_window_closes() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", params("31"), ms(0))
        .unwrap();
    assert_eq!(
        engine
            .observe_cleared("NODE_SILENT", "branch-12", ms(5_000))
            .unwrap(),
        ClearOutcome::Pending
    );

    // The abnormal state is gone, so the condition is not open, but nothing is recorded yet.
    assert!(engine.open().is_empty());
    assert_eq!(engine.sink().len(), 1);
    assert_eq!(engine.tick(ms(5_000 + DEBOUNCE - 1)).cleared, 0);
    assert_eq!(engine.sink().len(), 1);

    assert_eq!(engine.tick(ms(5_000 + DEBOUNCE)).cleared, 1);
    let events = engine.sink().events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].transition().as_str(), "cleared");
    assert_eq!(
        events[1].at_millis(),
        ms(5_000),
        "a debounced clear is recorded when it was observed, not when the window closed"
    );
    assert_eq!(events[1].severity(), Severity::Amber);
}

#[test]
fn observing_an_absent_condition_changes_nothing() {
    let mut engine = engine();
    assert_eq!(
        engine
            .observe_cleared("NODE_SILENT", "branch-12", ms(0))
            .unwrap(),
        ClearOutcome::AlreadyClear
    );
    engine
        .raise("NODE_SILENT", "branch-12", params("31"), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(1_000))
        .unwrap();
    // A second observation while the clear is pending does not move the recorded instant.
    assert_eq!(
        engine
            .observe_cleared("NODE_SILENT", "branch-12", ms(2_000))
            .unwrap(),
        ClearOutcome::AlreadyClear
    );
    engine.tick(ms(1_000 + DEBOUNCE));
    assert_eq!(engine.sink().len(), 2);
    assert_eq!(engine.sink().events()[1].at_millis(), ms(1_000));
}

#[test]
fn an_unregistered_id_is_refused_by_both_operations() {
    let mut engine = engine();
    let err = engine
        .raise(
            "NO_SUCH_CONDITION",
            "branch-12",
            ConditionParams::new(),
            ms(0),
        )
        .unwrap_err();
    assert_eq!(err.code(), "conditions.unknown_id");
    assert_eq!(
        err,
        ConditionsError::UnknownId {
            id: "NO_SUCH_CONDITION".to_owned()
        }
    );
    assert_eq!(
        engine
            .observe_cleared("NO_SUCH_CONDITION", "branch-12", ms(0))
            .unwrap_err(),
        ConditionsError::UnknownId {
            id: "NO_SUCH_CONDITION".to_owned()
        }
    );
}

#[test]
fn an_id_that_is_not_upper_snake_case_is_refused_before_the_registry_is_consulted() {
    let mut engine = engine();
    assert_eq!(
        engine
            .raise("node_silent", "branch-12", ConditionParams::new(), ms(0))
            .unwrap_err()
            .code(),
        "conditions.unknown_id"
    );
    assert!(engine.open().is_empty());
}

#[test]
fn history_is_the_transitions_of_one_subject_in_order() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", params("31"), ms(0))
        .unwrap();
    engine
        .raise(
            "JOURNAL_GAP",
            "branch-12",
            ConditionParams::new(),
            ms(1_000),
        )
        .unwrap();
    engine
        .raise("NODE_SILENT", "branch-13", params("2"), ms(2_000))
        .unwrap();

    let history = engine.history("branch-12");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].id.as_str(), "NODE_SILENT");
    assert_eq!(history[0].transition.as_str(), "raised");
    assert_eq!(history[0].at_millis, ms(0));
    assert_eq!(history[1].id.as_str(), "JOURNAL_GAP");
    assert_eq!(history[1].severity, Severity::Red);
    assert_eq!(history[1].at_millis, ms(1_000));

    assert_eq!(engine.history("branch-13").len(), 1);
    assert!(engine.history("branch-99").is_empty());
}

#[test]
fn open_lists_conditions_in_id_then_subject_order() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-2", params("1"), ms(0))
        .unwrap();
    engine
        .raise("NODE_SILENT", "branch-1", params("1"), ms(0))
        .unwrap();
    engine
        .raise("JOURNAL_GAP", "stream-9", ConditionParams::new(), ms(0))
        .unwrap();
    let open = engine.open();
    let order: Vec<(&str, &str)> = open
        .iter()
        .map(|condition| (condition.id.as_str(), condition.subject.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            ("JOURNAL_GAP", "stream-9"),
            ("NODE_SILENT", "branch-1"),
            ("NODE_SILENT", "branch-2"),
        ]
    );
}

#[test]
fn an_event_carries_the_values_of_the_transition_and_its_subject() {
    let mut engine = engine();
    engine
        .raise("JOURNAL_GAP", "segment-77", ConditionParams::new(), ms(42))
        .unwrap();
    let events = engine.sink().events();
    match &events[0] {
        AuditEvent::Raised {
            id,
            subject,
            severity,
            at_millis,
            params,
        } => {
            assert_eq!(id.as_str(), "JOURNAL_GAP");
            assert_eq!(subject.as_str(), "segment-77");
            assert_eq!(*severity, Severity::Red);
            assert_eq!(*at_millis, ms(42));
            assert!(params.is_empty());
        }
        other => panic!("expected a raised event, got {other:?}"),
    }
}
