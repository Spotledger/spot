//! Every code this crate raises has a catalogue entry, and the catalogue has no stale one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_request_authority::codes;

const ALL: [&str; 6] = [
    codes::COMMAND_ID_RESERVED,
    codes::IDENTIFIER_BLANK,
    codes::LIFETIME_TOO_LONG,
    codes::ATTEMPT_NOT_CURRENT,
    codes::SECRET_UNAVAILABLE,
    codes::ACTOR_NOT_TYPED,
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
        assert!(code.starts_with("bpm_request_authority."), "{code}");
        assert!(seen.insert(code), "duplicate code {code}");
        assert!(
            catalogue.lookup(code).is_some(),
            "no catalogue entry for `{code}`"
        );
    }
    let text = include_str!("../../spot-errors/catalogue/bpm_request_authority.toml");
    assert_eq!(
        text.lines().filter(|line| *line == "[[entry]]").count(),
        ALL.len(),
        "a catalogue entry has no code (or a code no entry)"
    );
}
