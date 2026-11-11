//! The catalogue: every `sandbox.*` code the crate defines has one entry with a shape the
//! platform's error catalogue accepts.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use serde::Deserialize;
use spot_sandbox::codes;

const CATALOGUE: &str = include_str!("../../spot-errors/catalogue/sandbox.toml");

#[derive(Deserialize)]
struct File {
    entry: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    code: String,
    class: String,
    summary: String,
    explanation_key: String,
    params: Vec<String>,
    remedy: String,
    since: String,
}

fn codes_in_crate() -> Vec<&'static str> {
    vec![
        codes::INVALID_PLAN,
        codes::POLICY,
        codes::STATE,
        codes::SNAPSHOT,
        codes::RESTORE,
        codes::TENANT,
        codes::STORE,
        codes::MASK,
        codes::SCAN_MISSING,
        codes::LEAK,
        codes::EFFECTS,
    ]
}

#[test]
fn every_code_has_one_entry() {
    let file: File = toml::from_str(CATALOGUE).unwrap();
    let catalogued: Vec<&str> = file.entry.iter().map(|entry| entry.code.as_str()).collect();
    for code in codes_in_crate() {
        assert_eq!(
            catalogued
                .iter()
                .filter(|candidate| **candidate == code)
                .count(),
            1,
            "{code} is not catalogued exactly once"
        );
    }
    assert_eq!(catalogued.len(), codes_in_crate().len());
}

#[test]
fn entries_have_a_complete_shape() {
    let file: File = toml::from_str(CATALOGUE).unwrap();
    for entry in file.entry {
        assert!(entry.code.starts_with("sandbox."));
        assert_eq!(entry.class, "none");
        assert_eq!(entry.explanation_key, format!("error.{}", entry.code));
        assert!(!entry.summary.is_empty());
        assert!(!entry.remedy.is_empty());
        assert!(entry.params.is_empty());
        assert_eq!(entry.since, "0.1.0");
        assert!(entry.summary.ends_with('.'));
        assert!(entry.remedy.ends_with('.'));
    }
}
