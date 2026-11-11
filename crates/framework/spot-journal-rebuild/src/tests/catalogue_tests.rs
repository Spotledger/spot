//! Every error code the crate returns is in the catalogue, and the catalogue holds nothing else.

use crate::RebuildError;

const CATALOGUE: &str = include_str!("../../../spot-errors/catalogue/journal_rebuild.toml");

fn every_error() -> Vec<RebuildError> {
    let s = String::new;
    vec![
        RebuildError::OptionsInvalid("x"),
        RebuildError::PlanInvalid("x"),
        RebuildError::VerifyFailed {
            kind: "k",
            generation: 1,
            first_seq: 1,
            last_seq: 1,
            count: 1,
        },
        RebuildError::VerifyUnavailable { code: "c" },
        RebuildError::ManifestInvalid {
            generation: 1,
            reason: "r",
        },
        RebuildError::SnapshotFailed { code: "c" },
        RebuildError::StoreFailed { code: "c" },
        RebuildError::SegmentMissing {
            generation: 1,
            first_seq: 1,
            last_seq: 1,
        },
        RebuildError::SegmentInvalid {
            generation: 1,
            first_seq: 1,
            last_seq: 1,
            reason: "r",
        },
        RebuildError::ForeignEntry {
            generation: 1,
            seq: 1,
        },
        RebuildError::EntryInvalid {
            generation: 1,
            seq: 1,
            reason: "r",
        },
        RebuildError::PositionGap {
            generation: 1,
            expected: 1,
            found: 2,
        },
        RebuildError::VersionRegressed {
            entity: s(),
            id: s(),
            generation: 1,
            seq: 1,
            applied: 2,
            found: 1,
        },
        RebuildError::MissingUpcast {
            entity: s(),
            from: 1,
            to: 2,
        },
        RebuildError::UpcastFailed {
            entity: s(),
            code: "c",
        },
        RebuildError::SinkFailed { reason: s() },
        RebuildError::Cancelled { processed: 1 },
    ]
}

#[test]
fn the_catalogue_and_the_codes_agree() {
    let in_catalogue: Vec<&str> = CATALOGUE
        .lines()
        .filter_map(|l| l.strip_prefix("code = \"")?.strip_suffix('"'))
        .collect();
    let mut in_code: Vec<&str> = every_error().iter().map(RebuildError::code).collect();
    in_code.sort_unstable();
    let mut sorted = in_catalogue.clone();
    sorted.sort_unstable();
    assert_eq!(in_code, sorted);
    assert!(in_code.iter().all(|c| c.starts_with("journal_rebuild.")));
}

#[test]
fn messages_name_ids_and_never_values() {
    for error in every_error() {
        let text = error.to_string();
        assert!(!text.is_empty());
        assert!(!text.contains('{'), "{text}");
    }
}
