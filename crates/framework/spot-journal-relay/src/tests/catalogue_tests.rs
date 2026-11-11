//! The error codes match the catalogue file.

use spot_journal_store::StoreError;

use crate::RelayError;

const CATALOGUE: &str = include_str!("../../../spot-errors/catalogue/journal_relay.toml");

#[test]
fn every_code_the_relay_raises_is_catalogued_and_nothing_else_is() {
    let all = [
        RelayError::ChainBroken { reason: "x" },
        RelayError::SplitBrain { reason: "x" },
        RelayError::EntryOutOfOrder {
            expected: 1,
            offered: 0,
        },
        RelayError::BatchGap {
            expected: 1,
            offered: 3,
        },
        RelayError::EntryInvalid("x"),
        RelayError::ConfigInvalid("x"),
        RelayError::ResumeRequired,
        RelayError::Store(StoreError::Backend {
            message: "x".to_owned(),
        }),
    ];
    let mut raised: Vec<&str> = all.iter().map(RelayError::code).collect();
    raised.sort_unstable();
    let mut catalogued: Vec<&str> = CATALOGUE
        .lines()
        .filter_map(|line| line.strip_prefix("code = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .collect();
    catalogued.sort_unstable();
    assert_eq!(raised, catalogued);
}

#[test]
fn the_five_codes_of_the_brief_exist() {
    for code in [
        "journal_relay.chain_broken",
        "journal_relay.split_brain",
        "journal_relay.entry_out_of_order",
        "journal_relay.batch_gap",
        "journal_relay.store_failed",
    ] {
        assert!(CATALOGUE.contains(&format!("code = \"{code}\"")), "{code}");
    }
}
