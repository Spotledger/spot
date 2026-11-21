//! The JSON Schema subset: derived from the field list, rendered as standard
//! JSON Schema, and checked against a payload without a schema library.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use spot_bpm_contract::{contracts, from_text};

#[test]
fn the_schema_is_derived_from_the_field_list() {
    let contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    let request = contract.message("request").expect("request");
    let schema = request.schema();
    assert_eq!(schema.message, "request");
    assert_eq!(schema.properties.len(), 3);
    assert_eq!(schema.required, vec!["reference", "amount_minor"]);

    let json = schema.to_json();
    assert_eq!(json["type"], json!("object"));
    assert_eq!(json["additionalProperties"], json!(false));
    assert_eq!(json["properties"]["reference"]["type"], json!("string"));
    assert_eq!(json["properties"]["reference"]["maxLength"], json!(64));
    assert_eq!(json["properties"]["amount_minor"]["type"], json!("integer"));
    assert_eq!(json["properties"]["note"]["type"], json!("string"));
    assert_eq!(json["required"], json!(["reference", "amount_minor"]));
}

#[test]
fn a_good_payload_validates() {
    let contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    let schema = contract.message("request").expect("request").schema();
    let payload = json!({
        "reference": "REQ-1",
        "amount_minor": 12_500,
        "note": "please",
    });
    assert!(schema.validate(&payload).is_empty());
    // An optional field may be absent.
    let payload = json!({ "reference": "REQ-1", "amount_minor": 1 });
    assert!(schema.validate(&payload).is_empty());
}

#[test]
fn a_missing_required_field_is_reported() {
    let contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    let schema = contract.message("request").expect("request").schema();
    let errors = schema.validate(&json!({ "reference": "REQ-1" }));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "amount_minor");
}

#[test]
fn a_field_the_contract_does_not_allow_never_crosses() {
    let contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    let schema = contract.message("request").expect("request").schema();
    let errors = schema.validate(&json!({
        "reference": "REQ-1",
        "amount_minor": 1,
        "salary": 100,
    }));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "salary");
}

#[test]
fn a_wrong_type_is_reported_and_a_float_is_never_an_integer() {
    let contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    let schema = contract.message("request").expect("request").schema();
    let errors = schema.validate(&json!({ "reference": 7, "amount_minor": "12" }));
    assert_eq!(errors.len(), 2);

    // ADR-12: money and quantities travel as integer minor units, so a float is
    // not an integer even when it has no fraction.
    let errors = schema.validate(&json!({ "reference": "REQ-1", "amount_minor": 12.5 }));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "amount_minor");
}

#[test]
fn a_text_longer_than_the_bound_is_reported() {
    let contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    let schema = contract.message("request").expect("request").schema();
    let long = "x".repeat(65);
    let errors = schema.validate(&json!({ "reference": long, "amount_minor": 1 }));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "reference");
}

#[test]
fn a_payload_that_is_not_an_object_is_reported() {
    let contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    let schema = contract.message("request").expect("request").schema();
    for payload in [json!("text"), json!([1, 2]), json!(null), json!(3)] {
        let errors = schema.validate(&payload);
        assert_eq!(errors.len(), 1, "{payload}");
        assert_eq!(errors[0].field, "");
    }
}

#[test]
fn every_problem_is_reported_at_once() {
    let contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    let schema = contract.message("request").expect("request").schema();
    let errors = schema.validate(&json!({ "reference": 7, "extra": true }));
    assert_eq!(errors.len(), 3, "{errors:#?}");
    let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
    assert!(fields.contains(&"amount_minor"));
    assert!(fields.contains(&"reference"));
    assert!(fields.contains(&"extra"));
}
