//! `ConditionEngine::restore` (WP-RTB-01.T2): a restart adopts a stored open set without
//! replaying any audit event, refuses a condition the registry no longer knows, refuses a
//! duplicate `(id, subject)`, and keeps the original escalation schedule.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    ConditionDef, ConditionEngine, ConditionParams, ConditionsError, Escalation, MemorySink,
    Millis, Open, Registry, Severity,
};

fn ms(millis: u64) -> Millis {
    Millis::new(millis)
}

fn node_silent_registry() -> Registry {
    let mut registry = Registry::new();
    registry
        .register(
            ConditionDef::new(
                "NODE_SILENT",
                Severity::Amber,
                "node",
                "condition.node_silent",
            )
            .with_escalation(Escalation::new(ms(3_600_000), Severity::Red)),
        )
        .unwrap();
    registry
}

#[test]
fn a_restored_engine_matches_the_open_set_it_was_given() {
    let mut source = ConditionEngine::new(node_silent_registry(), MemorySink::new());
    source
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new().with("silent_hours", "2"),
            ms(1_000),
        )
        .unwrap();
    let stored = source.open();
    assert_eq!(stored.len(), 1);

    let restored = ConditionEngine::restore(
        node_silent_registry(),
        MemorySink::new(),
        Millis::new(60_000),
        stored.clone(),
    )
    .unwrap();
    assert_eq!(restored.open(), stored);
    // A restore is not a transition: nothing is recorded to the sink.
    assert!(restored.sink().is_empty());
}

#[test]
fn restore_refuses_an_id_the_registry_does_not_know() {
    let open = vec![Open {
        id: spot_conditions::ConditionId::parse("MISSING_DEP").unwrap(),
        subject: "branch-12".into(),
        severity: Severity::Amber,
        params: ConditionParams::new(),
        raised_at_millis: ms(0),
        last_seen_millis: ms(0),
    }];
    let err = ConditionEngine::restore(
        node_silent_registry(),
        MemorySink::new(),
        Millis::new(60_000),
        open,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ConditionsError::UnknownId {
            id: "MISSING_DEP".to_owned()
        }
    );
}

#[test]
fn restore_refuses_two_entries_of_the_same_id_and_subject() {
    let one = Open {
        id: spot_conditions::ConditionId::parse("NODE_SILENT").unwrap(),
        subject: "branch-12".into(),
        severity: Severity::Amber,
        params: ConditionParams::new(),
        raised_at_millis: ms(0),
        last_seen_millis: ms(0),
    };
    let open = vec![one.clone(), one];
    let err = ConditionEngine::restore(
        node_silent_registry(),
        MemorySink::new(),
        Millis::new(60_000),
        open,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ConditionsError::DuplicateId {
            id: "NODE_SILENT".to_owned()
        }
    );
}

#[test]
fn a_restored_condition_escalates_on_its_original_schedule() {
    let open = vec![Open {
        id: spot_conditions::ConditionId::parse("NODE_SILENT").unwrap(),
        subject: "branch-12".into(),
        severity: Severity::Amber,
        params: ConditionParams::new(),
        raised_at_millis: ms(0),
        last_seen_millis: ms(0),
    }];
    let mut restored = ConditionEngine::restore(
        node_silent_registry(),
        MemorySink::new(),
        Millis::new(60_000),
        open,
    )
    .unwrap();

    // The hour is measured from the stored `raised_at`, not from the moment of the restore.
    assert_eq!(restored.tick(ms(3_599_999)).escalated, 0);
    assert_eq!(restored.open()[0].severity, Severity::Amber);
    assert_eq!(restored.tick(ms(3_600_000)).escalated, 1);
    assert_eq!(restored.open()[0].severity, Severity::Red);
    // The escalation itself is recorded normally: only the restore step records nothing.
    assert_eq!(restored.sink().len(), 1);
}
