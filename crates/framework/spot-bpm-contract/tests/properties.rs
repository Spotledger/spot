//! Mutation property tests over the contract model: a single edit either stays
//! valid or reports the specific code it should, arbitrary text never panics
//! the parser, and the content hash ignores definition order (brief §6).

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;

use spot_bpm_contract::{
    Field, FieldKind, MessageDef, Next, Outcome, OutcomeKind, Participant, ParticipantKind,
    ProcessContract, Signature, TimeoutRule, Transition, check, contracts, from_text, to_text,
};

/// The two-party contract every mutation starts from, without its signature:
/// a signature covers the content, so any content edit invalidates it and would
/// mask the code under test.
fn base() -> ProcessContract {
    let mut contract = from_text(contracts::REQUEST_APPROVE).unwrap();
    contract.signatures.clear();
    contract
}

/// Every mutation, and what it must report.
fn apply_mutation(index: usize, contract: &mut ProcessContract) -> Option<&'static str> {
    match index {
        0 => {
            contract.id = "Not A Name".to_owned();
            Some("bpm_contract.invalid_id")
        }
        1 => {
            contract.version = 0;
            Some("bpm_contract.version_zero")
        }
        2 => {
            contract.participants.clear();
            Some("bpm_contract.no_participants")
        }
        3 => {
            contract
                .participants
                .push(Participant::new("watcher", ParticipantKind::Tenant));
            Some("bpm_contract.role_inactive")
        }
        4 => {
            let duplicate = contract.participants[0].clone();
            contract.participants.push(duplicate);
            Some("bpm_contract.duplicate_role")
        }
        5 => {
            contract.messages.remove(0);
            Some("bpm_contract.unknown_message")
        }
        6 => {
            contract.messages[0].to = contract.messages[0].from.clone();
            Some("bpm_contract.self_message")
        }
        7 => {
            contract.messages[0].topic = "single".to_owned();
            Some("bpm_contract.invalid_topic")
        }
        8 => {
            contract.messages[0].fields[1].max_len = Some(8);
            Some("bpm_contract.schema")
        }
        9 => {
            contract.messages[0].fields[0].name = "NotAName".to_owned();
            Some("bpm_contract.invalid_field")
        }
        10 => {
            contract.order[1].by = "requester".to_owned();
            Some("bpm_contract.order_mismatch")
        }
        11 => {
            contract.order[1].next = Next::Message("ghost".to_owned());
            Some("bpm_contract.unknown_message")
        }
        12 => {
            contract.order[0].bound = Some(0);
            Some("bpm_contract.invalid_bound")
        }
        13 => {
            contract
                .order
                .push(Transition::message("requester", Some("approve"), "request"));
            Some("bpm_contract.unbounded_cycle")
        }
        14 => {
            contract
                .outcomes
                .push(Outcome::new("abandoned", OutcomeKind::Cancelled));
            Some("bpm_contract.outcome_unreachable")
        }
        15 => {
            contract.timeouts[0].seconds = 0;
            Some("bpm_contract.timeout")
        }
        16 => {
            let hash = contract.content_hash();
            contract.signatures.push(Signature::new(
                "nobody",
                "ed25519",
                "key",
                hash.as_hex(),
                "00ff",
            ));
            Some("bpm_contract.signature")
        }
        17 => {
            // A new message with a valid but new shape and its order rule: the
            // contract stays valid.
            contract.messages.push(MessageDef::new(
                "withdraw",
                "bpm.request",
                "requester",
                "approver",
                vec![Field::required("reference", FieldKind::String).with_max_len(64)],
            ));
            contract.order.push(Transition::message(
                "requester",
                Some("request"),
                "withdraw",
            ));
            contract.order.push(Transition::outcome(
                "approver",
                Some("withdraw"),
                "cancelled",
            ));
            None
        }
        18 => {
            contract.title = Some("Another title".to_owned());
            None
        }
        19 => {
            // A wait limit that only gets longer is still valid.
            contract
                .timeouts
                .push(TimeoutRule::new("cancel", "approver", 60));
            None
        }
        _ => None,
    }
}

proptest! {
    #[test]
    fn single_edit_mutations_stay_valid_or_report_their_code(index in 0usize..20) {
        let mut contract = base();
        let expected = apply_mutation(index, &mut contract);
        let problems = check(&contract);
        match expected {
            Some(code) => prop_assert!(
                problems.iter().any(|problem| problem.code == code),
                "mutation {index} expected {code}, got {problems:#?}"
            ),
            None => prop_assert!(
                problems.is_empty(),
                "mutation {index} expected a valid contract, got {problems:#?}"
            ),
        }
    }

    #[test]
    fn arbitrary_text_never_panics_the_parser(text in "\\PC{0,240}") {
        let _ = from_text(&text);
    }

    #[test]
    fn a_contract_that_parses_round_trips_its_text(text in "\\PC{0,600}") {
        if let Ok(contract) = from_text(&text) {
            let canonical = to_text(&contract);
            let parsed = from_text(&canonical).expect("canonical text parses");
            prop_assert_eq!(&parsed, &contract);
            prop_assert_eq!(to_text(&parsed), canonical);
        }
    }

    #[test]
    fn rotating_the_definition_order_keeps_the_content_hash(seed in any::<usize>()) {
        let contract = base();
        let mut rotated = contract.clone();
        rotate(&mut rotated.participants, seed);
        rotate(&mut rotated.messages, seed + 1);
        rotate(&mut rotated.order, seed + 2);
        rotate(&mut rotated.outcomes, seed + 3);
        rotate(&mut rotated.timeouts, seed + 4);
        prop_assert_eq!(contract.content_hash(), rotated.content_hash());
        prop_assert_eq!(check(&rotated), check(&contract));
    }
}

/// Rotates a list by `by` places, so a different definition order is built
/// without changing the set.
fn rotate<T>(items: &mut [T], by: usize) {
    if items.is_empty() {
        return;
    }
    let at = by % items.len();
    items.rotate_left(at);
}

#[test]
fn the_checker_is_deterministic_and_sorted() {
    let mut contract = base();
    contract.id = "Bad".to_owned();
    contract.version = 0;
    contract.participants.clear();
    let first = check(&contract);
    let second = check(&contract);
    assert_eq!(first, second);
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(first, sorted, "problems come back sorted");
}
