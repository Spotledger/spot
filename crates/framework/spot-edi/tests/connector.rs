//! Connector operation-set tests (WP-INT-06.T1, brief item 6).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use spot_connectors::ConnectorCall;
use spot_edi::{EdiConnector, Limits, codes, connector_id, parse_str};

fn connector() -> EdiConnector {
    EdiConnector {
        limits: Limits::default(),
    }
}

#[test]
fn describe_lists_the_four_operations() {
    let info = EdiConnector::describe();
    for name in ["parse", "build", "validate", "acknowledge"] {
        assert!(info.supports(name), "missing {name}");
    }
    assert_eq!(info.operations.len(), 4);
}

#[test]
fn parse_returns_the_interchange_and_its_envelope() {
    let output = connector()
        .invoke(
            "parse",
            &json!({"text": include_str!("corpus/x12_850.edi")}),
        )
        .unwrap();
    assert_eq!(output["interchange"]["control"], json!("000000001"));
    assert_eq!(output["envelope"], json!([]));
}

#[test]
fn build_round_trips_through_parse() {
    let text = include_str!("corpus/x12_850.edi");
    let parsed = connector().invoke("parse", &json!({"text": text})).unwrap();
    let rebuilt = connector()
        .invoke("build", &json!({"interchange": parsed["interchange"]}))
        .unwrap();
    assert_eq!(rebuilt["text"], json!(text));
}

#[test]
fn validate_reports_violations() {
    let schema = "x12_850";
    let good = connector()
        .invoke(
            "validate",
            &json!({"text": include_str!("corpus/x12_850.edi"), "schema": schema}),
        )
        .unwrap();
    assert_eq!(good["violations"], json!([]));

    let bad = include_str!("corpus/x12_850.edi").replace("BEG*00*NE*PO12345", "BEG*00*NE*");
    let output = connector()
        .invoke("validate", &json!({"text": bad, "schema": schema}))
        .unwrap();
    assert!(
        output["violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == codes::SCHEMA_BAD_ELEMENT),
        "{output}"
    );
}

#[test]
fn acknowledge_builds_a_997() {
    let output = connector()
        .invoke(
            "acknowledge",
            &json!({
                "text": include_str!("corpus/x12_850.edi"),
                "control": "000000009",
                "date": "240101",
                "time": "1200",
                "results": [{"set_id": "850", "control": "0001", "accepted": true, "errors": []}]
            }),
        )
        .unwrap();
    let text = output["text"].as_str().unwrap();
    let parsed = parse_str(text, Limits::default()).unwrap();
    assert_eq!(parsed.messages()[0].id(), "997");
}

#[test]
fn an_unknown_operation_is_refused() {
    let error = connector().invoke("nope", &json!({})).unwrap_err();
    assert_eq!(error.code(), codes::UNKNOWN_OPERATION);
}

#[test]
fn call_maps_errors_into_the_connector_vocabulary() {
    let id = connector_id().unwrap();
    let call = ConnectorCall {
        connector: id,
        operation: "nope".to_owned(),
        input: json!({}),
        tenant: "acme".to_owned(),
        actor: None,
        now_micros: 0,
        deadline_micros: 0,
    };
    let error = connector().call(&call).unwrap_err();
    assert_eq!(error.code(), spot_connectors::codes::UNKNOWN_OPERATION);

    let call = ConnectorCall {
        connector: connector_id().unwrap(),
        operation: "parse".to_owned(),
        input: json!({"text": "not edi"}),
        tenant: "acme".to_owned(),
        actor: None,
        now_micros: 0,
        deadline_micros: 0,
    };
    let error = connector().call(&call).unwrap_err();
    assert_eq!(error.code(), spot_connectors::codes::BAD_INPUT);
}

#[test]
fn a_caller_can_tighten_the_limits_but_never_raise_them() {
    let small = EdiConnector {
        limits: Limits::small(),
    };
    let text = include_str!("corpus/x12_850.edi").repeat(40);
    let raised = json!({
        "text": text,
        "limits": {"max_bytes": 1_000_000_000_u64, "max_segments": 1_000_000_000_u64, "max_elements": 1_000_000_000_u64}
    });
    assert!(small.invoke("parse", &raised).is_err());
}
