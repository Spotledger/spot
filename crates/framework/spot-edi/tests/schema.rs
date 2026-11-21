//! Schema validation tests (WP-INT-06.T1, brief item 4).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_edi::{Limits, Schema, codes, has_code, parse_str, validate_schema};

fn message(text: &str) -> spot_edi::Message {
    let interchange = parse_str(text, Limits::default()).unwrap();
    interchange
        .messages()
        .into_iter()
        .next()
        .expect("a message")
}

#[test]
fn every_embedded_schema_loads() {
    let ids = Schema::embedded_ids();
    assert!(ids.contains(&"x12_850"));
    assert!(ids.contains(&"edifact_orders"));
    for id in ids {
        Schema::load(id).unwrap_or_else(|e| panic!("{id} did not load: {e}"));
    }
}

#[test]
fn an_unknown_schema_id_is_an_error() {
    let error = Schema::load("x12_999").unwrap_err();
    assert_eq!(error.code(), codes::SCHEMA_UNKNOWN);
}

#[test]
fn a_bad_schema_document_is_an_error() {
    let error = Schema::from_toml("bad", "id = \"bad\"\nsyntax = \"x12\"\n").unwrap_err();
    assert_eq!(error.code(), codes::SCHEMA_BAD);
}

#[test]
fn the_corpus_documents_conform_to_their_schemas() {
    let cases = [
        ("x12_850", include_str!("corpus/x12_850.edi")),
        ("x12_810", include_str!("corpus/x12_810.edi")),
        ("x12_856", include_str!("corpus/x12_856.edi")),
        ("x12_997", include_str!("corpus/x12_997.edi")),
        ("edifact_orders", include_str!("corpus/edifact_orders.edi")),
        ("edifact_invoic", include_str!("corpus/edifact_invoic.edi")),
        ("edifact_desadv", include_str!("corpus/edifact_desadv.edi")),
        ("edifact_contrl", include_str!("corpus/edifact_contrl.edi")),
    ];
    for (id, text) in cases {
        let schema = Schema::load(id).unwrap();
        let violations = validate_schema(&message(text), &schema);
        assert!(violations.is_empty(), "{id} has violations: {violations:?}");
    }
}

#[test]
fn a_missing_required_segment_is_reported() {
    let schema = Schema::load("x12_850").unwrap();
    let mut message = message(include_str!("corpus/x12_850.edi"));
    message.segments.retain(|s| s.tag != "BEG");
    let violations = validate_schema(&message, &schema);
    assert!(
        has_code(&violations, codes::SCHEMA_MISSING_SEGMENT),
        "{violations:?}"
    );
}

#[test]
fn an_unexpected_segment_is_reported() {
    let schema = Schema::load("x12_850").unwrap();
    let mut message = message(include_str!("corpus/x12_850.edi"));
    message
        .segments
        .push(spot_edi::Segment::simple("ZZZ", vec!["x".into()]));
    let violations = validate_schema(&message, &schema);
    assert!(
        has_code(&violations, codes::SCHEMA_UNEXPECTED_SEGMENT),
        "{violations:?}"
    );
}

#[test]
fn a_bad_element_type_is_reported_with_its_position() {
    let schema = Schema::load("x12_850").unwrap();
    let mut message = message(include_str!("corpus/x12_850.edi"));
    let po1 = message
        .segments
        .iter_mut()
        .find(|s| s.tag == "PO1")
        .expect("a PO1");
    po1.set(1, "not-a-number");
    let violations = validate_schema(&message, &schema);
    let violation = violations
        .iter()
        .find(|v| v.code == codes::SCHEMA_BAD_ELEMENT)
        .expect("an element violation");
    assert_eq!(violation.position.tag, "PO1");
    assert_eq!(violation.position.element, Some(1));
}

#[test]
fn a_missing_required_element_is_reported() {
    let schema = Schema::load("x12_850").unwrap();
    let mut message = message(include_str!("corpus/x12_850.edi"));
    let beg = message
        .segments
        .iter_mut()
        .find(|s| s.tag == "BEG")
        .expect("a BEG");
    beg.set(2, "");
    let violations = validate_schema(&message, &schema);
    assert!(
        has_code(&violations, codes::SCHEMA_BAD_ELEMENT),
        "{violations:?}"
    );
}

#[test]
fn a_too_many_occurrences_is_reported() {
    let schema = Schema::from_toml(
        "tiny",
        r#"
id = "tiny"
syntax = "x12"
version = "0"
[[segment]]
tag = "ST"
min = 1
max = 1
[[segment]]
tag = "SE"
min = 1
max = 1
"#,
    )
    .unwrap();
    let message = spot_edi::Message {
        header: spot_edi::Segment::simple("ST", vec!["850".into(), "1".into()]),
        trailer: Some(spot_edi::Segment::simple(
            "SE",
            vec!["3".into(), "1".into()],
        )),
        segments: vec![spot_edi::Segment::simple(
            "ST",
            vec!["850".into(), "2".into()],
        )],
    };
    let violations = validate_schema(&message, &schema);
    assert!(
        has_code(&violations, codes::SCHEMA_TOO_MANY),
        "{violations:?}"
    );
}

#[test]
fn an_enum_element_must_be_one_of_its_values() {
    let schema = Schema::load("x12_997").unwrap();
    let mut message = message(include_str!("corpus/x12_997.edi"));
    let ak9 = message
        .segments
        .iter_mut()
        .find(|s| s.tag == "AK9")
        .expect("an AK9");
    ak9.set(0, "Z");
    let violations = validate_schema(&message, &schema);
    assert!(
        has_code(&violations, codes::SCHEMA_BAD_ELEMENT),
        "{violations:?}"
    );
}
