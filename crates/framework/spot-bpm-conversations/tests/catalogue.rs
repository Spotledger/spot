//! Every code this crate can raise has a catalogue entry, and the catalogue has no stale one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_conversations::codes;

const ALL: [&str; 29] = [
    codes::CONTRACT_UNKNOWN,
    codes::MESSAGE_UNKNOWN,
    codes::WRONG_DIRECTION,
    codes::OUT_OF_ORDER,
    codes::STEP_EXHAUSTED,
    codes::CONVERSATION_ENDED,
    codes::SCHEMA_UNSUPPORTED,
    codes::PAYLOAD_INVALID,
    codes::HEADER_INVALID,
    codes::WRONG_PEER,
    codes::NOT_ENTRY,
    codes::PAYLOAD_MISSING,
    codes::CONFIG_INVALID,
    codes::NO_CONVERSATION,
    codes::NO_PEER,
    codes::FIELD_NOT_GRANTED,
    codes::SEND_FAILED,
    codes::REJECTED,
    codes::WAIT_TIMEOUT,
    codes::DELIVERY_EXPIRED,
    codes::CONVERSATION_CANCELLED,
    codes::CASE_COLLISION,
    codes::AUDIT_FAILED,
    codes::LINK_CONVERSATION_LIMIT,
    codes::LINK_QUARANTINE_LIMIT,
    codes::STORE_FAILED,
    codes::CONFLICT,
    codes::TOO_LONG,
    codes::RESOLVE_FAILED,
];

#[test]
fn every_code_has_a_catalogue_entry_and_the_catalogue_has_nothing_else() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(
        catalogue.load_error(),
        None,
        "the catalogue must load clean"
    );
    let mut seen = std::collections::BTreeSet::new();
    for code in ALL {
        assert!(code.starts_with("bpm_conversations."), "{code}");
        assert!(seen.insert(code), "duplicate code {code}");
        assert!(
            catalogue.lookup(code).is_some(),
            "no catalogue entry for `{code}`"
        );
    }
    let text = include_str!("../../spot-errors/catalogue/bpm_conversations.toml");
    assert_eq!(
        text.lines().filter(|line| *line == "[[entry]]").count(),
        ALL.len(),
        "a catalogue entry has no code (or a code no entry)"
    );
}
