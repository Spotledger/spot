//! The shipped contracts: the text form round-trips byte for byte, the checker
//! accepts them, the hash ignores definition order, and signatures are stored,
//! verified and never required (ADR-080 decision 4).

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use spot_bpm_contract::{
    AcceptAnySignature, Participant, ProcessContract, RefuseEverySignature, Signature, codes,
    contracts, diff, from_text, is_compatible, to_text, verify_signatures,
};

/// The contract directory.
fn contract_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contracts")
}

#[test]
fn every_contract_file_is_listed_and_round_trips() {
    let dir = contract_dir();
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").file_name())
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.ends_with(".toml"))
        .collect();
    names.sort();
    let listed: Vec<String> = contracts::ALL
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    assert_eq!(
        names, listed,
        "contracts/ and contracts::ALL are out of sync"
    );

    for (name, text) in contracts::ALL {
        let contract = from_text(text).unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
        assert_eq!(
            to_text(&contract),
            text,
            "{name} is not in canonical text form"
        );
        assert_eq!(
            from_text(&to_text(&contract)).unwrap(),
            contract,
            "{name} does not round-trip"
        );
        let problems = spot_bpm_contract::check(&contract);
        assert!(
            problems.is_empty(),
            "{name} does not pass the checker: {problems:#?}"
        );
        assert!(
            diff(&contract, &contract).is_empty(),
            "{name} differs from itself"
        );
        assert!(is_compatible(&contract, &contract));
    }
}

#[test]
fn a_contract_hashes_the_same_whatever_the_definition_order_or_title() {
    let text = contracts::REQUEST_APPROVE;
    let contract = from_text(text).unwrap();
    let mut reordered = contract.clone();
    reordered.participants.reverse();
    reordered.messages.reverse();
    reordered.order.reverse();
    reordered.outcomes.reverse();
    reordered.timeouts.reverse();
    assert_eq!(contract.content_hash(), reordered.content_hash());

    // The title is descriptive, like a process version's.
    reordered.title = Some("Another title".to_owned());
    assert_eq!(contract.content_hash(), reordered.content_hash());

    // A signature covers the hash, so it cannot be part of it.
    reordered.signatures.clear();
    assert_eq!(contract.content_hash(), reordered.content_hash());

    // A real change moves it.
    reordered.messages[0].topic = "bpm.other".to_owned();
    assert_ne!(contract.content_hash(), reordered.content_hash());
}

#[test]
fn signatures_that_are_present_are_verified_and_none_are_required() {
    let signed = from_text(contracts::REQUEST_APPROVE).unwrap();
    assert_eq!(signed.signatures.len(), 1, "the golden carries a signature");
    assert_eq!(
        signed.signatures[0].signed_hash,
        signed.content_hash().as_hex()
    );
    assert!(
        verify_signatures(&signed, &AcceptAnySignature).is_empty(),
        "a present signature verifies"
    );
    let refused = verify_signatures(&signed, &RefuseEverySignature);
    assert_eq!(
        refused
            .iter()
            .filter(|p| p.code == codes::SIGNATURE)
            .count(),
        1
    );

    // A contract with no signatures is valid: requiring one is the caller's
    // policy, not this crate's.
    let unsigned = from_text(contracts::THREE_PARTY_RELAY).unwrap();
    assert!(unsigned.signatures.is_empty());
    assert!(verify_signatures(&unsigned, &RefuseEverySignature).is_empty());
    assert!(spot_bpm_contract::check(&unsigned).is_empty());
}

#[test]
fn a_signature_over_another_version_is_refused() {
    let mut contract = from_text(contracts::THREE_PARTY_RELAY).unwrap();
    contract.signatures.push(Signature::new(
        "buyer",
        "ed25519",
        "buyer-key-1",
        "0".repeat(64),
        "abc",
    ));
    let problems = spot_bpm_contract::check(&contract);
    assert!(
        problems.iter().any(|p| p.code == codes::SIGNATURE),
        "{problems:#?}"
    );
    // The TOML round-trips the signature the checker refused.
    assert_eq!(from_text(&to_text(&contract)).unwrap(), contract);
}

#[test]
fn the_three_party_relay_forwards_as_a_new_message() {
    let contract = from_text(contracts::THREE_PARTY_RELAY).unwrap();
    let order = contract.message("order").expect("order");
    let forward = contract.message("forward").expect("forward");
    assert_eq!(
        (order.from.as_str(), order.to.as_str()),
        ("buyer", "broker")
    );
    assert_eq!(
        (forward.from.as_str(), forward.to.as_str()),
        ("broker", "supplier"),
        "the broker sends a new message on the next link; the platform does not relay"
    );
    assert_eq!(
        order.topic, forward.topic,
        "the same topic on the next link"
    );
    assert!(
        contract
            .participants
            .iter()
            .any(|p| p.role == "supplier" && p.kind == spot_bpm_contract::ParticipantKind::External)
    );
    assert_eq!(contract.participants.len(), 3);
}

#[test]
fn a_hand_written_contract_without_a_title_parses() {
    let text = r#"
id = "minimal"
version = 1

[[participants]]
role = "a"
kind = "tenant"

[[participants]]
role = "b"
kind = "tenant"

[[messages]]
name = "hi"
topic = "x.y"
schema_version = 1
from = "a"
to = "b"

[[order]]
by = "a"
next = "hi"

[[order]]
by = "b"
after = "hi"
outcome = "done"

[[outcomes]]
name = "done"
kind = "completed"
"#;
    let contract: ProcessContract = from_text(text).expect("minimal contract parses");
    assert_eq!(contract.title, None);
    assert!(contract.signatures.is_empty());
    assert_eq!(
        spot_bpm_contract::check(&contract),
        Vec::new(),
        "a minimal contract is complete"
    );
    assert_eq!(
        contract.participants,
        vec![
            Participant::new("a", spot_bpm_contract::ParticipantKind::Tenant),
            Participant::new("b", spot_bpm_contract::ParticipantKind::Tenant),
        ]
    );
}
