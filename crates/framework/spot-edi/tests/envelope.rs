//! Envelope validation tests (WP-INT-06.T1, brief item 2).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_edi::{Limits, check_envelope, codes, has_code, parse_str};

fn x12() -> String {
    include_str!("corpus/x12_850.edi").to_owned()
}

fn edifact() -> String {
    include_str!("corpus/edifact_orders.edi").to_owned()
}

#[test]
fn a_consistent_x12_interchange_has_no_violations() {
    let interchange = parse_str(&x12(), Limits::default()).unwrap();
    assert!(check_envelope(&interchange).is_empty());
}

#[test]
fn a_wrong_interchange_group_count_is_reported() {
    let text = x12().replace("IEA*1*000000001", "IEA*2*000000001");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_COUNT),
        "{violations:?}"
    );
}

#[test]
fn a_wrong_interchange_control_number_is_reported() {
    let text = x12().replace("IEA*1*000000001", "IEA*1*000000009");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_CONTROL),
        "{violations:?}"
    );
}

#[test]
fn a_wrong_transaction_set_count_is_reported() {
    let text = x12().replace("SE*10*0001", "SE*9*0001");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_COUNT),
        "{violations:?}"
    );
}

#[test]
fn a_wrong_transaction_set_control_number_is_reported() {
    let text = x12().replace("SE*10*0001", "SE*10*0002");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_CONTROL),
        "{violations:?}"
    );
}

#[test]
fn a_wrong_group_count_is_reported() {
    let text = x12().replace("GE*1*1", "GE*3*1");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_COUNT),
        "{violations:?}"
    );
}

#[test]
fn a_missing_transaction_trailer_is_reported_as_unbalanced() {
    let text = x12().replace("SE*10*0001~", "");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::UNBALANCED_ENVELOPE),
        "{violations:?}"
    );
}

#[test]
fn a_consistent_edifact_interchange_has_no_violations() {
    let interchange = parse_str(&edifact(), Limits::default()).unwrap();
    assert!(check_envelope(&interchange).is_empty());
}

#[test]
fn an_edifact_wrong_message_count_is_reported() {
    let text = edifact().replace("UNZ+1+1", "UNZ+2+1");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_COUNT),
        "{violations:?}"
    );
}

#[test]
fn an_edifact_wrong_message_control_is_reported() {
    let text = edifact().replace("UNT+11+1", "UNT+11+9");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_CONTROL),
        "{violations:?}"
    );
}

#[test]
fn an_edifact_grouped_interchange_validates_its_group_envelope() {
    let grouped = "UNA:+.? 'UNB+UNOA:1+SENDER:ZZ+RECEIVER:ZZ+20240101:1200+5'UNG+ORDERS+SENDER+RECEIVER+20240101:1200+7+UN+D:96A'UNH+1+ORDERS:D:96A:UN'BGM+220+PO1+9'UNT+3+1'UNE+1+7'UNZ+1+5'";
    let interchange = parse_str(grouped, Limits::default()).unwrap();
    assert!(
        check_envelope(&interchange).is_empty(),
        "{:?}",
        check_envelope(&interchange)
    );
    assert_eq!(interchange.groups().len(), 1);

    let wrong_group_control = grouped.replace("UNE+1+7", "UNE+1+8");
    let interchange = parse_str(&wrong_group_control, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_CONTROL),
        "{violations:?}"
    );

    let wrong_group_count = grouped.replace("UNE+1+7", "UNE+2+7");
    let interchange = parse_str(&wrong_group_count, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    assert!(
        has_code(&violations, codes::ENVELOPE_COUNT),
        "{violations:?}"
    );
}

#[test]
fn a_not_edi_input_fails() {
    let error = parse_str("hello, world", Limits::default()).unwrap_err();
    assert_eq!(error.code(), codes::NOT_EDI);
}

#[test]
fn a_truncated_isa_fails_with_bad_delimiters() {
    let error = parse_str("ISA*00*", Limits::default()).unwrap_err();
    assert_eq!(error.code(), codes::BAD_DELIMITERS);
}

#[test]
fn an_interchange_without_a_trailer_fails() {
    let text = x12().replace("IEA*1*000000001~", "");
    let error = parse_str(&text, Limits::default()).unwrap_err();
    assert_eq!(error.code(), codes::MISSING_ENVELOPE);
}

#[test]
fn positions_are_reported_with_the_violation() {
    let text = x12().replace("SE*10*0001", "SE*9*0001");
    let interchange = parse_str(&text, Limits::default()).unwrap();
    let violations = check_envelope(&interchange);
    let violation = violations
        .iter()
        .find(|v| v.code == codes::ENVELOPE_COUNT)
        .expect("a count violation");
    assert_eq!(violation.position.tag, "SE");
    assert!(violation.position.offset > 0);
    assert!(violation.to_string().contains("SE"));
}
