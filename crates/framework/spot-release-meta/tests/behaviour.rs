#![allow(missing_docs)]

//! The behaviour changelog: keys, parsing, rendering and the L5 divergence check.

use spot_release_meta::{
    BehaviourChangelog, BehaviourEntry, BehaviourKey, BehaviourKind, Divergence, Sensitivity, codes,
};

const CHANGELOG: &str = "\
[[entry]]
key = \"doctype:Sales Invoice\"
summary = \"Tax is rounded to the invoice currency\"
since_version = \"1.2.0\"
kind = \"rounding\"
sensitivity = \"high\"

[[entry]]
key = \"seam:accounts:posting\"
summary = \"Posting writes one extra line\"
since_version = \"1.2.0\"
kind = \"posting\"
sensitivity = \"medium\"
";

#[test]
fn keys_parse_and_render() {
    let doctype = match BehaviourKey::parse("doctype:Sales Invoice") {
        Ok(key) => key,
        Err(error) => panic!("valid key refused: {error}"),
    };
    assert_eq!(doctype, BehaviourKey::DocType("Sales Invoice".to_owned()));
    assert_eq!(doctype.text(), "doctype:Sales Invoice");

    let seam = match BehaviourKey::parse("seam:accounts:posting") {
        Ok(key) => key,
        Err(error) => panic!("valid key refused: {error}"),
    };
    assert_eq!(seam, BehaviourKey::Seam("accounts:posting".to_owned()));
    assert_eq!(seam.text(), "seam:accounts:posting");
}

#[test]
fn bad_keys_are_refused() {
    let error = match BehaviourKey::parse("sales invoice") {
        Ok(key) => panic!("expected an error, got {key:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), codes::BAD_KEY);
    assert!(BehaviourKey::parse("table:x").is_err());
    assert!(BehaviourKey::parse("doctype:").is_err());
}

#[test]
fn the_changelog_round_trips() {
    let changelog = match BehaviourChangelog::from_toml(CHANGELOG) {
        Ok(changelog) => changelog,
        Err(error) => panic!("valid changelog refused: {error}"),
    };
    assert_eq!(changelog.entries.len(), 2);
    let first = changelog
        .entry(&BehaviourKey::DocType("Sales Invoice".to_owned()))
        .copied_or_panic();
    assert_eq!(first.kind, BehaviourKind::Rounding);
    assert_eq!(first.sensitivity, Sensitivity::High);
    assert!(first.sensitivity.is_high());

    let text = changelog.render();
    let again = match BehaviourChangelog::from_toml(&text) {
        Ok(again) => again,
        Err(error) => panic!("rendered changelog refused: {error}"),
    };
    assert_eq!(changelog, again);
    assert_eq!(text, again.render(), "render is not stable");
}

#[test]
fn a_malformed_changelog_is_refused() {
    let error = match BehaviourChangelog::from_toml("[[entry]]\nkey = \"doctype:X\"\n") {
        Ok(changelog) => panic!("expected an error, got {changelog:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), codes::BAD_CHANGELOG);
}

#[test]
fn diff_against_returns_only_undeclared_divergences() {
    let changelog = match BehaviourChangelog::from_toml(CHANGELOG) {
        Ok(changelog) => changelog,
        Err(error) => panic!("valid changelog refused: {error}"),
    };
    let divergences = vec![
        Divergence {
            key: BehaviourKey::DocType("Sales Invoice".to_owned()),
            summary: "declared, must not appear".to_owned(),
        },
        Divergence {
            key: BehaviourKey::DocType("Purchase Order".to_owned()),
            summary: "undeclared".to_owned(),
        },
        Divergence {
            key: BehaviourKey::Seam("stock:valuation".to_owned()),
            summary: "undeclared".to_owned(),
        },
    ];
    let undeclared = changelog.diff_against(&divergences);
    let keys: Vec<String> = undeclared.iter().map(|d| d.key.text()).collect();
    assert_eq!(
        keys,
        vec![
            "doctype:Purchase Order".to_owned(),
            "seam:stock:valuation".to_owned()
        ]
    );
}

#[test]
fn the_unit_source_reports_nothing() {
    let changelog = BehaviourChangelog::default();
    assert!(changelog.diff_against(&()).is_empty());
}

/// Small helper so the tests read well and still avoid `unwrap`.
trait CopiedOrPanic {
    fn copied_or_panic(self) -> BehaviourEntry;
}

impl CopiedOrPanic for Option<&BehaviourEntry> {
    fn copied_or_panic(self) -> BehaviourEntry {
        match self {
            Some(entry) => entry.clone(),
            None => panic!("entry not found"),
        }
    }
}
