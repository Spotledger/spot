//! Every error code the crate returns is in the catalogue, and the catalogue holds nothing else.

use spot_journal_pii::PiiError;

use crate::ProtectError;

const CATALOGUE: &str = include_str!("../../../spot-errors/catalogue/journal_protect.toml");

fn every_error() -> Vec<ProtectError> {
    vec![
        ProtectError::SpecUnavailable {
            reason: "x".to_owned(),
        },
        ProtectError::EntryInvalid("x"),
        ProtectError::IdentityMismatch("x"),
        ProtectError::Protect(PiiError::KeyMissing),
        ProtectError::Reveal(PiiError::SubjectMismatch),
    ]
}

#[test]
fn the_catalogue_and_the_codes_agree() {
    let mut in_catalogue: Vec<&str> = CATALOGUE
        .lines()
        .filter_map(|l| l.strip_prefix("code = \"")?.strip_suffix('"'))
        .collect();
    in_catalogue.sort_unstable();
    let mut in_code: Vec<&str> = every_error().iter().map(ProtectError::code).collect();
    in_code.sort_unstable();
    assert_eq!(in_code, in_catalogue);
    assert!(in_code.iter().all(|c| c.starts_with("journal_protect.")));
}
