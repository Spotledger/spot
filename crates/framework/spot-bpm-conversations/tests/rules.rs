//! The pure rules: order, wire format and the message-arrived matcher.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;

use common::contract;
use serde_json::json;
use spot_bpm_contract::{ProcessContract, Transition};
use spot_bpm_conversations::order::{
    OrderState, check_entry, check_message, ending_after, take_step, timeout_outcome,
};
use spot_bpm_conversations::wire::{Header, decode, encode};
use spot_bpm_conversations::{Violation, message_arrived_spec};

fn state<'a>(
    last: Option<&'a str>,
    counts: &'a BTreeMap<String, u32>,
    ended: bool,
) -> OrderState<'a> {
    OrderState {
        last,
        counts,
        ended,
    }
}

fn request() -> serde_json::Value {
    json!({"reference": "R1", "amount_minor": 5})
}

#[test]
fn the_order_rules_decide_what_may_follow() {
    let c = contract();
    let none = BTreeMap::new();
    let check = |from: &str, to: &str, name: &str, last, p: &serde_json::Value| {
        check_message(&c, from, to, name, 1, p, &state(last, &none, false)).map(|d| d.name.clone())
    };
    assert_eq!(
        check("requester", "approver", "request", None, &request()),
        Ok("request".into())
    );
    assert_eq!(
        check(
            "requester",
            "approver",
            "cancel",
            None,
            &json!({"reference": "R"})
        ),
        Err(Violation::OutOfOrder)
    );
    assert_eq!(
        check(
            "approver",
            "requester",
            "approve",
            Some("request"),
            &json!({"reference": "R"})
        ),
        Ok("approve".into())
    );
    assert_eq!(
        check(
            "requester",
            "approver",
            "request",
            Some("request"),
            &request()
        ),
        Err(Violation::OutOfOrder)
    );
    assert_eq!(
        check("approver", "requester", "request", None, &request()),
        Err(Violation::WrongDirection)
    );
    assert_eq!(
        check("requester", "approver", "nope", None, &request()),
        Err(Violation::MessageUnknown)
    );
    assert!(matches!(
        check_message(
            &c,
            "requester",
            "approver",
            "request",
            2,
            &request(),
            &state(None, &none, false)
        ),
        Err(Violation::SchemaUnsupported)
    ));
    assert_eq!(
        check_message(
            &c,
            "requester",
            "approver",
            "request",
            1,
            &request(),
            &state(None, &none, true)
        )
        .map(|_| ()),
        Err(Violation::ConversationEnded)
    );
}

#[test]
fn a_step_can_be_taken_only_as_often_as_the_contract_allows() {
    let c = contract();
    let mut counts = BTreeMap::new();
    take_step(&c, None, "requester", "request", &mut counts);
    let again = check_message(
        &c,
        "requester",
        "approver",
        "request",
        1,
        &request(),
        &state(None, &counts, false),
    );
    assert_eq!(again.map(|_| ()), Err(Violation::StepExhausted));
    // A bound lifts the limit to that many.
    let mut bounded: ProcessContract = c.clone();
    for t in &mut bounded.order {
        if t.after.is_none() {
            *t = Transition::message("requester", None, "request").with_bound(2);
        }
    }
    assert!(
        check_message(
            &bounded,
            "requester",
            "approver",
            "request",
            1,
            &request(),
            &state(None, &counts, false)
        )
        .is_ok()
    );
}

#[test]
fn only_a_starting_message_opens_a_conversation() {
    let c = contract();
    assert!(check_entry(&c, "requester", "request").is_ok());
    assert_eq!(
        check_entry(&c, "requester", "cancel"),
        Err(Violation::NotEntry)
    );
    assert_eq!(
        check_entry(&c, "approver", "request"),
        Err(Violation::NotEntry)
    );
}

#[test]
fn the_ending_of_a_conversation_follows_the_contract() {
    let c = contract();
    assert_eq!(ending_after(&c, "approve").unwrap().name, "approved");
    assert_eq!(ending_after(&c, "reject").unwrap().name, "rejected");
    assert_eq!(ending_after(&c, "cancel").unwrap().name, "cancelled");
    assert!(
        ending_after(&c, "request").is_none(),
        "a request can still be answered"
    );
    assert_eq!(
        timeout_outcome(&c, "approve", "requester").unwrap().name,
        "expired"
    );
    assert!(timeout_outcome(&c, "approve", "approver").is_none());
}

#[test]
fn a_reason_never_echoes_a_value_or_a_name_a_peer_chose() {
    let c = contract();
    let none = BTreeMap::new();
    let payload = json!({
        "reference": "TOPSECRET".repeat(20),
        "amount_minor": "TOPSECRET",
        "peerchosenname": "TOPSECRET"
    });
    let error = check_message(
        &c,
        "requester",
        "approver",
        "request",
        1,
        &payload,
        &state(None, &none, false),
    )
    .unwrap_err();
    let reason = error.reason();
    assert!(
        !reason.contains("TOPSECRET") && !reason.contains("peerchosenname"),
        "{reason}"
    );
    assert!(
        reason.contains("reference") && reason.contains("amount_minor"),
        "{reason}"
    );
    assert!(
        reason.contains("1 field"),
        "unknown names are counted, not echoed: {reason}"
    );
}

#[test]
fn the_wire_form_round_trips_and_is_bounded() {
    let c = contract();
    let def = c.message("request").unwrap();
    let header = Header::new("request_approve", 1, "request");
    let wire = encode(
        &header,
        &json!({"reference": "R1", "amount_minor": 5, "note": "n"}),
        def,
    )
    .unwrap();
    let text = serde_json::to_string(&wire).unwrap();
    let decoded = decode(&text).unwrap();
    assert_eq!(decoded.header, header);
    assert_eq!(
        decoded.payload,
        json!({"reference": "R1", "amount_minor": 5, "note": "n"})
    );
    // Anything the contract does not describe is never encoded.
    assert!(encode(&header, &json!({"reference": "R1", "extra": 1}), def).is_err());
    assert!(encode(&header, &json!({"reference": 1}), def).is_err());

    assert_eq!(Header::parse("request_approve@1/request"), Some(header));
    for bad in [
        "",
        "a",
        "a@1",
        "a@/b",
        "a@0/b",
        "a@x/b",
        "A@1/b",
        "a@1/B",
        "a@99999999999/b",
    ] {
        assert_eq!(Header::parse(bad), None, "{bad}");
    }
    assert_eq!(Header::parse(&format!("{}@1/b", "a".repeat(200))), None);
    assert_eq!(decode("not json"), Err(Violation::HeaderInvalid));
    assert_eq!(decode("{}"), Err(Violation::HeaderInvalid));
}

#[test]
fn the_message_arrived_spec_names_its_contract_and_message() {
    let spec = message_arrived_spec("request_approve", "request");
    assert_eq!(spec.kind(), "message_arrived");
}
