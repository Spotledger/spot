//! A contract is untrusted input from a partner: every collection and every
//! text has a bound, and going past one is a typed `bpm_contract.too_large`
//! problem or a refused document, never a slow check or a large allocation.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_contract::limits::{
    MAX_FIELDS_PER_MESSAGE, MAX_MESSAGES, MAX_ORDER_RULES, MAX_OUTCOMES, MAX_ROLES, MAX_SIGNATURES,
    MAX_STRING_BYTES, MAX_TEXT_BYTES, MAX_TIMEOUTS,
};
use spot_bpm_contract::{
    Field, FieldKind, MessageDef, Outcome, OutcomeKind, Participant, ParticipantKind,
    ProcessContract, Signature, TextError, TimeoutRule, Transition, check, codes, conforms,
    contracts, from_text, to_text,
};

fn base() -> ProcessContract {
    from_text(contracts::REQUEST_APPROVE).unwrap()
}

fn too_large(contract: &ProcessContract) -> bool {
    check(contract).iter().any(|p| p.code == codes::TOO_LARGE)
}

#[test]
fn a_shipped_contract_is_within_every_bound() {
    assert!(!too_large(&base()));
}

#[test]
fn too_many_roles_are_refused() {
    let mut contract = base();
    for i in 0..=MAX_ROLES {
        contract
            .participants
            .push(Participant::new(format!("r{i}"), ParticipantKind::External));
    }
    assert!(too_large(&contract));
}

#[test]
fn too_many_messages_are_refused() {
    let mut contract = base();
    for i in 0..=MAX_MESSAGES {
        contract.messages.push(MessageDef::new(
            format!("m{i}"),
            "demo.topic",
            "requester",
            "approver",
            vec![],
        ));
    }
    assert!(too_large(&contract));
}

#[test]
fn too_many_fields_in_a_message_are_refused() {
    let mut contract = base();
    contract.messages[0].fields = (0..=MAX_FIELDS_PER_MESSAGE)
        .map(|i| Field::required(format!("f{i}"), FieldKind::Integer))
        .collect();
    assert!(too_large(&contract));
}

#[test]
fn too_many_order_rules_outcomes_timeouts_and_signatures_are_refused() {
    let mut contract = base();
    contract.order = (0..=MAX_ORDER_RULES)
        .map(|_| Transition::message("requester", None, "request"))
        .collect();
    assert!(too_large(&contract));

    let mut contract = base();
    contract.outcomes = (0..=MAX_OUTCOMES)
        .map(|i| Outcome::new(format!("o{i}"), OutcomeKind::Completed))
        .collect();
    assert!(too_large(&contract));

    let mut contract = base();
    contract.timeouts = (0..=MAX_TIMEOUTS)
        .map(|_| TimeoutRule::new("approve", "requester", 60))
        .collect();
    assert!(too_large(&contract));

    let mut contract = base();
    contract.signatures = (0..=MAX_SIGNATURES)
        .map(|_| Signature::new("requester", "ed25519", "key", "0".repeat(64), "sig"))
        .collect();
    assert!(too_large(&contract));
}

#[test]
fn an_overlong_name_or_title_is_refused() {
    let mut contract = base();
    contract.title = Some("t".repeat(MAX_STRING_BYTES + 1));
    assert!(too_large(&contract));

    let mut contract = base();
    contract.messages[0].topic = "a.".repeat(MAX_STRING_BYTES);
    assert!(too_large(&contract));
}

#[test]
fn an_oversize_contract_reports_only_the_size_problem_and_does_no_further_work() {
    let mut contract = base();
    for i in 0..=MAX_ROLES {
        contract
            .participants
            .push(Participant::new(format!("r{i}"), ParticipantKind::External));
    }
    let problems = check(&contract);
    assert!(
        problems.iter().all(|p| p.code == codes::TOO_LARGE),
        "{problems:#?}"
    );
}

#[test]
fn conformance_refuses_an_oversize_contract() {
    let mut contract = base();
    contract.messages[0].fields = (0..=MAX_FIELDS_PER_MESSAGE)
        .map(|i| Field::required(format!("f{i}"), FieldKind::Integer))
        .collect();
    let version = spot_bpm_model::ProcessVersion {
        name: "p".to_owned(),
        app: "demo".to_owned(),
        title: None,
        version: 1,
        status: spot_bpm_model::VersionStatus::Draft,
        nodes: vec![],
        edges: vec![],
        rules: vec![],
    };
    let problems = conforms(&version, &contract, "requester");
    assert!(
        problems.iter().any(|p| p.code == codes::TOO_LARGE),
        "{problems:#?}"
    );
}

#[test]
fn oversize_text_is_refused_before_it_is_parsed() {
    // Not even valid TOML: the size is refused first.
    let text = "x".repeat(MAX_TEXT_BYTES + 1);
    assert!(matches!(from_text(&text), Err(TextError::TooLarge { .. })));
}

#[test]
fn text_with_too_many_items_is_refused_when_read() {
    let mut contract = base();
    contract.outcomes = (0..=MAX_OUTCOMES)
        .map(|i| Outcome::new(format!("o{i}"), OutcomeKind::Completed))
        .collect();
    let text = to_text(&contract);
    assert!(text.len() <= MAX_TEXT_BYTES);
    assert!(matches!(from_text(&text), Err(TextError::TooLarge { .. })));
}

#[test]
fn text_at_the_bounds_still_reads() {
    let mut contract = base();
    contract.outcomes = (0..MAX_OUTCOMES)
        .map(|i| Outcome::new(format!("o{i}"), OutcomeKind::Completed))
        .collect();
    assert!(from_text(&to_text(&contract)).is_ok());
}
