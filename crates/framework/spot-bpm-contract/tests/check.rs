//! The checker: one test per problem kind, and every problem reported at once.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use spot_bpm_contract::{
    ClassifiedFields, Field, FieldClass, FieldKind, MessageDef, Next, Outcome, OutcomeKind,
    Participant, ParticipantKind, ProcessContract, Signature, TimeoutRule, Transition, check,
    check_with, codes,
};

/// A valid two-party, two-message contract.
fn base() -> ProcessContract {
    ProcessContract::new("ping_pong", 1)
        .with_title("Ping and pong")
        .with_participants(vec![
            Participant::new("a", ParticipantKind::Tenant),
            Participant::new("b", ParticipantKind::External),
        ])
        .with_messages(vec![
            MessageDef::new(
                "ping",
                "demo.ping",
                "a",
                "b",
                vec![Field::required("reference", FieldKind::String).with_max_len(32)],
            ),
            MessageDef::new(
                "pong",
                "demo.pong",
                "b",
                "a",
                vec![Field::required("reference", FieldKind::String).with_max_len(32)],
            ),
        ])
        .with_order(vec![
            Transition::message("a", None, "ping"),
            Transition::message("b", Some("ping"), "pong"),
            Transition::outcome("a", Some("pong"), "done"),
        ])
        .with_outcomes(vec![Outcome::new("done", OutcomeKind::Completed)])
        .with_timeouts(vec![TimeoutRule::new("pong", "a", 60)])
}

/// The codes a contract's check produced.
fn codes_of(contract: &ProcessContract) -> BTreeSet<&'static str> {
    check(contract).into_iter().map(|p| p.code).collect()
}

#[test]
fn a_valid_contract_has_no_problems() {
    assert_eq!(check(&base()), Vec::new());
}

#[test]
fn every_problem_is_reported_at_once() {
    let mut contract = base();
    contract.id = "Not A Name".to_owned();
    contract.version = 0;
    contract.participants.clear();
    let found = codes_of(&contract);
    assert!(found.contains(codes::INVALID_ID), "{found:?}");
    assert!(found.contains(codes::VERSION_ZERO), "{found:?}");
    assert!(found.contains(codes::NO_PARTICIPANTS), "{found:?}");
}

#[test]
fn an_invalid_id_or_version_is_reported() {
    let mut contract = base();
    contract.id = "Bad Id".to_owned();
    assert!(codes_of(&contract).contains(codes::INVALID_ID));

    let mut contract = base();
    contract.version = 0;
    assert!(codes_of(&contract).contains(codes::VERSION_ZERO));
}

#[test]
fn a_role_that_can_never_act_is_reported() {
    let mut contract = base();
    contract
        .participants
        .push(Participant::new("watcher", ParticipantKind::Tenant));
    assert!(codes_of(&contract).contains(codes::ROLE_INACTIVE));
}

#[test]
fn an_invalid_or_duplicate_role_is_reported() {
    let mut contract = base();
    contract.participants[0].role = "A".to_owned();
    assert!(codes_of(&contract).contains(codes::INVALID_ROLE));

    let mut contract = base();
    let duplicate = contract.participants[0].clone();
    contract.participants.push(duplicate);
    assert!(codes_of(&contract).contains(codes::DUPLICATE_ROLE));
}

#[test]
fn an_invalid_duplicate_or_self_message_is_reported() {
    let mut contract = base();
    contract.messages[0].name = "not a name".to_owned();
    assert!(codes_of(&contract).contains(codes::INVALID_MESSAGE));

    let mut contract = base();
    contract.messages.push(contract.messages[0].clone());
    assert!(codes_of(&contract).contains(codes::DUPLICATE_MESSAGE));

    let mut contract = base();
    contract.messages[0].to = "a".to_owned();
    assert!(codes_of(&contract).contains(codes::SELF_MESSAGE));
}

#[test]
fn an_unknown_role_or_invalid_topic_on_a_message_is_reported() {
    let mut contract = base();
    contract.messages[0].from = "nobody".to_owned();
    assert!(codes_of(&contract).contains(codes::UNKNOWN_ROLE));

    let mut contract = base();
    contract.messages[0].topic = "single".to_owned();
    assert!(codes_of(&contract).contains(codes::INVALID_TOPIC));
}

#[test]
fn schema_errors_are_reported() {
    let mut contract = base();
    contract.messages[0].schema_version = 0;
    assert!(codes_of(&contract).contains(codes::SCHEMA));

    let mut contract = base();
    contract.messages[0].fields =
        vec![Field::required("amount", FieldKind::Integer).with_max_len(8)];
    assert!(codes_of(&contract).contains(codes::SCHEMA));

    let mut contract = base();
    contract.messages[0].fields =
        vec![Field::required("amount", FieldKind::String).with_max_len(0)];
    assert!(codes_of(&contract).contains(codes::SCHEMA));

    let mut contract = base();
    contract.messages[0].fields = vec![Field::required("NotAName", FieldKind::String)];
    assert!(codes_of(&contract).contains(codes::INVALID_FIELD));

    let mut contract = base();
    let field = Field::required("reference", FieldKind::String);
    contract.messages[0].fields = vec![field.clone(), field];
    assert!(codes_of(&contract).contains(codes::DUPLICATE_FIELD));
}

#[test]
fn a_field_the_metadata_classifies_as_secret_is_refused() {
    let contract = base();
    let meta = ClassifiedFields::new().with("reference", FieldClass::Secret);
    let found: BTreeSet<&str> = check_with(&contract, &meta)
        .into_iter()
        .map(|p| p.code)
        .collect();
    assert!(found.contains(codes::SECRET_FIELD), "{found:?}");
    // Without metadata the same contract is clean.
    assert!(check(&contract).is_empty());
    // A personal field is allowed: the contract lists it by name.
    let personal = ClassifiedFields::new().with("reference", FieldClass::Personal);
    assert!(check_with(&contract, &personal).is_empty());
}

#[test]
fn an_unknown_message_or_outcome_in_the_order_is_reported() {
    let mut contract = base();
    contract.order[1].after = Some("ghost".to_owned());
    assert!(codes_of(&contract).contains(codes::UNKNOWN_MESSAGE));

    let mut contract = base();
    contract.order[1].next = Next::Message("ghost".to_owned());
    assert!(codes_of(&contract).contains(codes::UNKNOWN_MESSAGE));

    let mut contract = base();
    contract.order[2].next = Next::Outcome("ghost".to_owned());
    assert!(codes_of(&contract).contains(codes::UNKNOWN_OUTCOME));
}

#[test]
fn an_order_rule_whose_role_is_not_the_sender_is_reported() {
    let mut contract = base();
    contract.order[1].by = "a".to_owned();
    assert!(codes_of(&contract).contains(codes::ORDER_MISMATCH));

    let mut contract = base();
    contract.order[1].by = "nobody".to_owned();
    assert!(codes_of(&contract).contains(codes::UNKNOWN_ROLE));
}

#[test]
fn a_duplicate_or_zero_bound_rule_is_reported() {
    let mut contract = base();
    contract.order.push(contract.order[0].clone());
    assert!(codes_of(&contract).contains(codes::DUPLICATE_TRANSITION));

    let mut contract = base();
    contract.order[0].bound = Some(0);
    assert!(codes_of(&contract).contains(codes::INVALID_BOUND));
}

#[test]
fn an_unreachable_message_is_reported() {
    let mut contract = base();
    contract.messages.push(MessageDef::new(
        "orphan",
        "demo.orphan",
        "a",
        "b",
        vec![Field::required("reference", FieldKind::String)],
    ));
    assert!(codes_of(&contract).contains(codes::MESSAGE_UNREACHABLE));
}

#[test]
fn a_dead_end_without_an_outcome_is_reported() {
    let mut contract = base();
    contract.order.pop();
    let found = codes_of(&contract);
    assert!(found.contains(codes::DEAD_END), "{found:?}");
    assert!(found.contains(codes::OUTCOME_UNREACHABLE), "{found:?}");
}

#[test]
fn an_outcome_no_rule_leads_to_is_reported() {
    let mut contract = base();
    contract
        .outcomes
        .push(Outcome::new("abandoned", OutcomeKind::Cancelled));
    assert!(codes_of(&contract).contains(codes::OUTCOME_UNREACHABLE));
}

#[test]
fn a_cycle_without_a_bound_is_reported_and_a_bounded_one_is_allowed() {
    let mut contract = base();
    contract
        .order
        .push(Transition::message("b", Some("pong"), "ping"));
    assert!(codes_of(&contract).contains(codes::UNBOUNDED_CYCLE));

    let mut contract = base();
    contract
        .order
        .push(Transition::message("b", Some("pong"), "ping").with_bound(3));
    assert!(
        !codes_of(&contract).contains(&codes::UNBOUNDED_CYCLE),
        "a bounded revision loop is how a conversation repeats"
    );
}

#[test]
fn wait_limits_are_checked() {
    let mut contract = base();
    contract.timeouts[0].message = "ghost".to_owned();
    assert!(codes_of(&contract).contains(codes::TIMEOUT));

    let mut contract = base();
    contract.timeouts[0].by = "b".to_owned();
    assert!(
        codes_of(&contract).contains(codes::TIMEOUT),
        "the waiter must be the receiver"
    );

    let mut contract = base();
    contract.timeouts[0].seconds = 0;
    assert!(codes_of(&contract).contains(codes::TIMEOUT));

    let mut contract = base();
    contract.timeouts[0].outcome = Some("ghost".to_owned());
    assert!(codes_of(&contract).contains(codes::TIMEOUT));

    let mut contract = base();
    contract.timeouts.push(contract.timeouts[0].clone());
    assert!(codes_of(&contract).contains(codes::TIMEOUT));
}

#[test]
fn signatures_are_checked_structurally() {
    let contract = base();
    let hash = contract.content_hash();

    let mut signed = contract.clone();
    signed.signatures.push(Signature::new(
        "a",
        "ed25519",
        "a-key",
        hash.as_hex(),
        "00ff",
    ));
    assert_eq!(check(&signed), Vec::new());

    let mut unknown_signer = contract.clone();
    unknown_signer.signatures.push(Signature::new(
        "nobody",
        "ed25519",
        "n-key",
        hash.as_hex(),
        "00ff",
    ));
    assert!(codes_of(&unknown_signer).contains(codes::SIGNATURE));

    let mut stale = contract.clone();
    stale.signatures.push(Signature::new(
        "a",
        "ed25519",
        "a-key",
        "0".repeat(64),
        "00ff",
    ));
    assert!(codes_of(&stale).contains(codes::SIGNATURE));

    let mut empty = contract.clone();
    empty
        .signatures
        .push(Signature::new("a", "", "", hash.as_hex(), ""));
    assert!(codes_of(&empty).contains(codes::SIGNATURE));

    let mut twice = contract;
    twice.signatures.push(Signature::new(
        "a",
        "ed25519",
        "a-key",
        hash.as_hex(),
        "00ff",
    ));
    twice.signatures.push(Signature::new(
        "a",
        "ed25519",
        "a-key-2",
        hash.as_hex(),
        "11ee",
    ));
    assert!(codes_of(&twice).contains(codes::SIGNATURE));
}
