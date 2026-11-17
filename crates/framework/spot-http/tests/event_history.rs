//! The selectors and `served_from` helpers of the event stream routes (contract 0.16.0): each
//! status and field, against plain JSON bodies.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use spot_http::api::ports::event_history::{
    ClassIn, MAX_ENTITIES, SelectorsIn, ServedFrom, parse_selectors,
};

/// The status and the `error.code` of the refusal of `body`.
fn refusal(body: &serde_json::Value) -> (u16, String) {
    let error = parse_selectors(body).unwrap_err();
    let envelope = error.to_envelope_json("req-1");
    (
        error.status().as_u16(),
        envelope["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

#[test]
fn an_absent_classes_is_domain_and_audit() {
    let parsed = parse_selectors(&json!({"topics": ["x.*"]})).unwrap();
    assert_eq!(
        parsed,
        SelectorsIn {
            classes: vec![ClassIn::Domain, ClassIn::Audit],
            entities: vec![],
            classes_named: false,
        }
    );
}

#[test]
fn state_with_entities_is_accepted_in_contract_order() {
    let parsed =
        parse_selectors(&json!({"classes": ["audit", "state"], "entities": ["invoice", "party"]}))
            .unwrap();
    assert_eq!(parsed.classes, vec![ClassIn::State, ClassIn::Audit]);
    assert_eq!(parsed.entities, vec!["invoice", "party"]);
    assert!(parsed.classes_named);
    assert!(parsed.classes.iter().any(|c| c.is_reserved()));
}

#[test]
fn bad_selectors_are_422_spec_invalid() {
    for body in [
        json!({"classes": ["telemetry"]}),
        json!({"classes": ["bogus"]}),
        json!({"classes": ["state", "state"]}),
        json!({"classes": []}),
        json!({"classes": "state"}),
        json!({"classes": [1]}),
        json!({"entities": ["invoice"]}),
        json!({"classes": ["domain"], "entities": ["invoice"]}),
        json!({"classes": ["state"], "entities": ["a", "a"]}),
        json!({"classes": ["state"], "entities": [""]}),
        json!({"classes": ["state"], "entities": "invoice"}),
        json!({"classes": ["state"], "entities": (0..=MAX_ENTITIES).map(|i| format!("e{i}")).collect::<Vec<_>>()}),
        json!({"classes": ["state"], "entities": ["x".repeat(201)]}),
    ] {
        assert_eq!(
            refusal(&body),
            (422, "eventstream.spec_invalid".to_owned()),
            "{body}"
        );
    }
}

#[test]
fn served_from_is_the_contracts_text_in_every_place() {
    assert_eq!(ServedFrom::Journal.as_str(), "journal");
    assert_eq!(ServedFrom::Outbox.header_value(), "outbox");
    assert_eq!(ServedFrom::Journal.member(), json!("journal"));
    let frame = String::from_utf8(ServedFrom::Outbox.frame()).unwrap();
    assert_eq!(
        frame,
        "event: served_from\ndata: {\"served_from\":\"outbox\"}\n\n"
    );
}
