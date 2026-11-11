//! The error codes match the catalogue file.

use crate::CommitError;

const CATALOGUE: &str = include_str!("../../../spot-errors/catalogue/journal_commit.toml");

#[test]
fn every_code_the_driver_raises_is_catalogued_and_nothing_else_is() {
    let all = [
        CommitError::SeqOutOfOrder {
            expected: 1,
            offered: 2,
        },
        CommitError::UnknownSeq { seq: 1 },
        CommitError::RelayStopped { cause: "x" },
    ];
    let mut raised: Vec<&str> = all.iter().map(CommitError::code).collect();
    raised.sort_unstable();
    let mut catalogued: Vec<&str> = CATALOGUE
        .lines()
        .filter_map(|line| line.strip_prefix("code = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .collect();
    catalogued.sort_unstable();
    assert_eq!(raised, catalogued);
}
