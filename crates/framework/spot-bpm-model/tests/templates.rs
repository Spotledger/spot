//! Every template in `templates/` parses, validates and compiles (brief §5).

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use spot_bpm_model::{AllowAllMeta, Limits, compile, from_text, templates, validate_version};

/// The template directory.
fn template_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates")
}

#[test]
fn every_template_file_is_listed_and_compiles() {
    let dir = template_dir();
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").file_name())
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.ends_with(".toml"))
        .collect();
    names.sort();

    let listed: Vec<String> = templates::ALL
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    assert_eq!(
        names, listed,
        "templates/ and templates::ALL are out of sync"
    );

    for (name, text) in templates::ALL {
        let version = from_text(text).unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
        let problems = validate_version(&version, &AllowAllMeta, &Limits::new());
        assert!(
            problems.is_empty(),
            "{name} does not validate: {problems:#?}"
        );
        let compiled =
            compile(&version).unwrap_or_else(|e| panic!("{name} does not compile: {e:#?}"));
        assert_eq!(compiled.version, version.version);
        assert_eq!(compiled.nodes.len(), version.nodes.len());
        assert_eq!(compiled.definition.nodes.len(), version.nodes.len());
        assert!(compiled.definition.edges.len() >= version.edges.len());
    }
}

#[test]
fn every_template_round_trips_through_text() {
    for (name, text) in templates::ALL {
        let version = from_text(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let canonical = spot_bpm_model::to_text(&version);
        let reparsed = from_text(&canonical).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(reparsed, version, "{name} does not round-trip");
        assert_eq!(
            spot_bpm_model::to_text(&reparsed),
            canonical,
            "{name} text form is not idempotent"
        );
    }
}

#[test]
fn the_template_library_is_not_empty_and_covers_the_brief() {
    assert!(templates::ALL.len() >= 4);
    for (name, _) in templates::ALL {
        assert!(name.ends_with(".toml"));
    }
}
