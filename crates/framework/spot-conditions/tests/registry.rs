//! The registry's refusals and what an app-registered condition gets.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    ConditionDef, ConditionsError, Escalation, Millis, Registry, ResponseStep, Severity,
};

fn def(id: &str) -> ConditionDef {
    ConditionDef::new(id, Severity::Amber, "node", "condition.test")
}

#[test]
fn a_registered_definition_is_retrievable_by_its_id() {
    let mut registry = Registry::new();
    registry.register(def("MISSING_DEP")).unwrap();
    let found = registry.get("MISSING_DEP").unwrap();
    assert_eq!(found.id, "MISSING_DEP");
    assert_eq!(found.severity, Severity::Amber);
    assert_eq!(found.subject_kind, "node");
    assert_eq!(registry.len(), 1);
    assert!(registry.get("missing_dep").is_none());
    assert!(registry.get("NO_SUCH_CONDITION").is_none());
}

#[test]
fn a_duplicate_id_is_refused_and_the_first_definition_stays() {
    let mut registry = Registry::new();
    registry.register(def("LAG_HIGH")).unwrap();
    let err = registry.register(def("LAG_HIGH")).unwrap_err();
    assert_eq!(err.code(), "conditions.duplicate_id");
    assert_eq!(
        err,
        ConditionsError::DuplicateId {
            id: "LAG_HIGH".to_owned()
        }
    );
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get("LAG_HIGH").unwrap().severity, Severity::Amber);
}

#[test]
fn an_id_that_is_not_upper_snake_case_is_refused() {
    let mut registry = Registry::new();
    for id in [
        "",
        "missing_dep",
        "Missing_Dep",
        "MISSING-DEP",
        " MISSING_DEP",
        "_MISSING_DEP",
        "MISSING_DEP_",
        "MISSING__DEP",
        "1MISSING",
        "MISSING DEP",
        "MISSING.DEP",
    ] {
        let err = registry.register(def(id)).unwrap_err();
        assert_eq!(
            err.code(),
            "conditions.unknown_id",
            "id {id:?} was accepted"
        );
    }
    assert!(registry.is_empty());
}

#[test]
fn an_id_at_the_length_limit_is_accepted_and_one_past_it_is_not() {
    let mut registry = Registry::new();
    let at_limit = "A".repeat(spot_conditions::ConditionId::MAX_LEN);
    registry.register(def(&at_limit)).unwrap();
    let past_limit = "A".repeat(spot_conditions::ConditionId::MAX_LEN + 1);
    assert_eq!(
        registry.register(def(&past_limit)).unwrap_err().code(),
        "conditions.unknown_id"
    );
}

#[test]
fn an_app_registers_its_own_condition_through_the_same_api() {
    let mut registry = Registry::new();
    registry.register(def("JOURNAL_GAP")).unwrap();
    // An app's own condition (ADR-067 s1: "a projector failure"), with everything a platform
    // condition has.
    registry
        .register(
            ConditionDef::new(
                "PROJECTOR_FAILED",
                Severity::Red,
                "record",
                "condition.projector",
            )
            .with_param("projector")
            .with_response(ResponseStep::new("retry_apply", 1))
            .with_escalation(Escalation::new(Millis::new(3_600_000), Severity::Red))
            .with_action("admin.retry_projector"),
        )
        .unwrap();

    let app = registry.get("PROJECTOR_FAILED").unwrap();
    assert_eq!(app.severity, Severity::Red);
    assert_eq!(app.params, vec!["projector".to_owned()]);
    assert_eq!(app.response, vec![ResponseStep::new("retry_apply", 1)]);
    assert_eq!(app.actions, vec!["admin.retry_projector".to_owned()]);
    assert_eq!(registry.len(), 2);
}

#[test]
fn registration_puts_the_escalation_schedule_in_elapsed_time_order() {
    let mut registry = Registry::new();
    registry
        .register(
            def("NODE_SILENT")
                .with_escalation(Escalation::new(Millis::new(24 * 3_600_000), Severity::Red))
                .with_escalation(Escalation::new(Millis::new(3_600_000), Severity::Amber)),
        )
        .unwrap();
    let schedule: Vec<u64> = registry
        .get("NODE_SILENT")
        .unwrap()
        .escalation
        .iter()
        .map(|step| step.after_millis.as_u64())
        .collect();
    assert_eq!(schedule, vec![3_600_000, 24 * 3_600_000]);
}

#[test]
fn the_definition_reports_the_parameters_a_raise_left_out() {
    let def = def("CLOCK_SKEW")
        .with_param("offset_minutes")
        .with_param("node");
    let params = spot_conditions::ConditionParams::new().with("node", "branch-12");
    assert_eq!(def.missing_params(&params), vec!["offset_minutes"]);
    assert!(
        def.missing_params(&params.with("offset_minutes", "7"))
            .is_empty()
    );
}
