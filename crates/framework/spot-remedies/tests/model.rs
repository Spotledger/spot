//! The remedy vocabulary: ids, parameter schemas, plan hashing and undo tokens.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use proptest::prelude::*;
use serde_json::json;
use spot_remedies::{
    ParamSchema, ParamType, Plan, PlanHash, PlanStep, RemedyId, UndoKind, UndoToken,
};

#[test]
fn remedy_ids_are_dotted_lower_case() {
    assert!(RemedyId::is_valid("job.retry_failed"));
    assert!(RemedyId::is_valid("quarantine.retry_all"));
    assert!(!RemedyId::is_valid("job"));
    assert!(!RemedyId::is_valid("Job.retry"));
    assert!(!RemedyId::is_valid("job..retry"));
    assert!(!RemedyId::is_valid(".job"));
    assert!(!RemedyId::is_valid("job."));
    assert!(!RemedyId::is_valid("job.Retry"));
    assert!(RemedyId::parse("bad").is_err());
}

#[test]
fn a_schema_rejects_a_float_and_an_unknown_key() {
    let schema = ParamSchema::empty()
        .with_required("count", ParamType::Integer)
        .with_optional("label", ParamType::String);

    assert!(schema.validate(&json!({"count": 3})).is_ok());
    assert!(schema.validate(&json!({"count": 3, "label": "x"})).is_ok());

    // A float is never accepted (money and quantities are integers).
    assert!(schema.validate(&json!({"count": 1.5})).is_err());
    // A wrong type.
    assert!(schema.validate(&json!({"count": "3"})).is_err());
    // A missing required parameter.
    assert!(schema.validate(&json!({"label": "x"})).is_err());
    // An unknown key with a closed schema.
    assert!(
        schema
            .validate(&json!({"count": 3, "extra": true}))
            .is_err()
    );
    // Not an object at all.
    assert!(schema.validate(&json!([1, 2])).is_err());
}

#[test]
fn the_json_schema_document_round_trips_the_shape() {
    let schema = ParamSchema::empty().with_required("count", ParamType::Integer);
    let document = schema.to_json_schema();
    assert_eq!(document["type"], json!("object"));
    assert_eq!(document["additionalProperties"], json!(false));
    assert_eq!(document["required"], json!(["count"]));
    assert_eq!(document["properties"]["count"]["type"], json!("integer"));
}

#[test]
fn a_sealed_plan_verifies_and_tampering_breaks_it() {
    let remedy = RemedyId::new("job.retry_failed");
    let params = json!({"job_id": "j1"});
    let steps = vec![PlanStep::new("job:j1", "job_retry", "retry the job")];
    let plan = Plan::seal(remedy.clone(), params.clone(), steps.clone());
    assert!(plan.verify());

    let mut tampered_params = plan.clone();
    tampered_params.params = json!({"job_id": "j2"});
    assert!(!tampered_params.verify());

    let mut tampered_steps = plan.clone();
    tampered_steps
        .steps
        .push(PlanStep::new("job:j9", "job_retry", "extra"));
    assert!(!tampered_steps.verify());

    let mut tampered_remedy = plan.clone();
    tampered_remedy.remedy = RemedyId::new("cache.flush_namespace");
    assert!(!tampered_remedy.verify());
}

#[test]
fn the_plan_hash_is_deterministic() {
    let remedy = RemedyId::new("job.retry_failed");
    let params = json!({"b": 2, "a": 1});
    let steps = vec![PlanStep::new("job:j1", "job_retry", "retry")];
    assert_eq!(
        PlanHash::compute(&remedy, &params, &steps),
        PlanHash::compute(&remedy, &params, &steps)
    );
}

#[test]
fn undo_tokens_are_distinct_by_sequence() {
    let hash = PlanHash::compute(&RemedyId::new("a.b"), &json!({}), &[]);
    assert_ne!(
        UndoToken::derive(&hash, 10, 1),
        UndoToken::derive(&hash, 10, 2)
    );
    assert_eq!(
        UndoToken::derive(&hash, 10, 1),
        UndoToken::derive(&hash, 10, 1)
    );
}

#[test]
fn undo_kind_reports_reversibility() {
    assert!(UndoKind::reversible("restore").is_reversible());
    assert!(!UndoKind::irreversible("no way back").is_reversible());
}

proptest! {
    /// Any sealed plan verifies, whatever its remedy, parameters and steps.
    #[test]
    fn sealed_plans_always_verify(
        remedy in "[a-z][a-z0-9_]*\\.[a-z][a-z0-9_]*",
        value in 0i64..10_000,
        target in "[a-z]{1,8}",
    ) {
        let remedy = RemedyId::new(&remedy);
        let params = json!({"value": value});
        let steps = vec![PlanStep::new(&target, "op_name", "a step")];
        let plan = Plan::seal(remedy, params, steps);
        prop_assert!(plan.verify());
    }
}
