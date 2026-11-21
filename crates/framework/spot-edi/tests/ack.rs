//! Acknowledgement tests (WP-INT-06.T1, brief item 5).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_edi::{
    AckRequest, Limits, TransactionResult, acknowledge, check_envelope, parse_str, results_from,
    write,
};

fn accepted_request() -> AckRequest {
    let mut request = AckRequest::new("000000009", "240101", "1200");
    request.functional_group = Some("PO".to_owned());
    request.group_control = Some("9".to_owned());
    request.results = vec![TransactionResult::accepted("850", "0001")];
    request
}

#[test]
fn a_997_acknowledges_an_x12_interchange() {
    let original = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let ack = acknowledge(&original, &accepted_request()).unwrap();
    let text = write(&ack).unwrap();
    let parsed = parse_str(&text, Limits::default()).unwrap();
    assert!(
        check_envelope(&parsed).is_empty(),
        "{:?}",
        check_envelope(&parsed)
    );
    assert_eq!(parsed.control, "000000009");
    // The acknowledgement goes back to the original sender.
    assert_eq!(parsed.sender.id, "RECEIVER");
    assert_eq!(parsed.receiver.id, "SENDER");
    let message = parsed.messages().into_iter().next().unwrap();
    assert_eq!(message.id(), "997");
    let ak1 = message.segments.iter().find(|s| s.tag == "AK1").unwrap();
    assert_eq!(ak1.text(0), "PO");
    let ak5 = message.segments.iter().find(|s| s.tag == "AK5").unwrap();
    assert_eq!(ak5.text(0), "A");
    let ak9 = message.segments.iter().find(|s| s.tag == "AK9").unwrap();
    assert_eq!(ak9.text(0), "A");
    assert_eq!(ak9.text(1), "1");
}

#[test]
fn a_997_rejects_with_error_codes() {
    let original = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let mut request = accepted_request();
    request.results = vec![TransactionResult::rejected(
        "850",
        "0001",
        vec!["edi.envelope_count".to_owned()],
    )];
    let ack = acknowledge(&original, &request).unwrap();
    let message = ack.messages().into_iter().next().unwrap();
    let ak5 = message.segments.iter().find(|s| s.tag == "AK5").unwrap();
    assert_eq!(ak5.text(0), "R");
    let ak4 = message.segments.iter().find(|s| s.tag == "AK4").unwrap();
    assert_eq!(ak4.text(2), "edi.envelope_count");
}

#[test]
fn a_contrl_acknowledges_an_edifact_interchange() {
    let original = parse_str(include_str!("corpus/edifact_orders.edi"), Limits::default()).unwrap();
    let mut request = AckRequest::new("9", "20240101", "1200");
    request.results = vec![TransactionResult::accepted("ORDERS", "1")];
    let ack = acknowledge(&original, &request).unwrap();
    let text = write(&ack).unwrap();
    let parsed = parse_str(&text, Limits::default()).unwrap();
    assert!(
        check_envelope(&parsed).is_empty(),
        "{:?}",
        check_envelope(&parsed)
    );
    let message = parsed.messages().into_iter().next().unwrap();
    assert_eq!(message.id(), "CONTRL");
    let uci = message.segments.iter().find(|s| s.tag == "UCI").unwrap();
    assert_eq!(uci.text(0), "1");
    assert_eq!(uci.text(3), "7");
    let ucm = message.segments.iter().find(|s| s.tag == "UCM").unwrap();
    assert_eq!(ucm.text(0), "1");
    assert_eq!(ucm.text(2), "7");
}

#[test]
fn an_edifact_rejection_carries_a_ucd() {
    let original = parse_str(include_str!("corpus/edifact_orders.edi"), Limits::default()).unwrap();
    let mut request = AckRequest::new("9", "20240101", "1200");
    request.results = vec![TransactionResult::rejected(
        "ORDERS",
        "1",
        vec!["edi.schema_bad_element".to_owned()],
    )];
    let ack = acknowledge(&original, &request).unwrap();
    let message = ack.messages().into_iter().next().unwrap();
    let ucd = message.segments.iter().find(|s| s.tag == "UCD").unwrap();
    assert_eq!(ucd.text(0), "edi.schema_bad_element");
    let ucm = message.segments.iter().find(|s| s.tag == "UCM").unwrap();
    assert_eq!(ucm.text(2), "8");
}

#[test]
fn the_acknowledgement_round_trips() {
    let original = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let ack = acknowledge(&original, &accepted_request()).unwrap();
    let text = write(&ack).unwrap();
    let again = write(&parse_str(&text, Limits::default()).unwrap()).unwrap();
    assert_eq!(text, again);
}

#[test]
fn results_from_reads_the_messages() {
    let original = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let results = results_from(&original, true);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].set_id, "850");
    assert_eq!(results[0].control, "0001");
    assert!(results[0].accepted);
}
