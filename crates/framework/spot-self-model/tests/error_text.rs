#![allow(clippy::expect_used, clippy::unwrap_used)]

//! The words of `self_model.query_denied` must not reveal a tier or a minimum: the service
//! answers a node behind the caller's tier floor exactly like a missing node.

use spot_self_model::{SelfModelError, codes};

const CATALOGUE: &str = include_str!("../../spot-errors/catalogue/self_model.toml");

fn assert_clean(text: &str, what: &str) {
    let lower = text.to_lowercase();
    for word in ["tier", "minimum"] {
        assert!(!lower.contains(word), "{what} mentions `{word}`: {text}");
    }
}

#[test]
fn query_denied_display_reveals_no_tier() {
    let error = SelfModelError::QueryDenied {
        node: "doctype:invoice".into(),
    };
    assert_eq!(error.code(), "self_model.query_denied");
    assert_eq!(error.code(), codes::QUERY_DENIED);
    assert_clean(&error.to_string(), "the Display text");
}

#[test]
fn query_denied_catalogue_text_reveals_no_tier() {
    let doc: toml::Table = CATALOGUE.parse().expect("the catalogue parses");
    let entries = doc["entry"].as_array().expect("entries");
    let entry = entries
        .iter()
        .find(|e| e["code"].as_str() == Some("self_model.query_denied"))
        .expect("the code has a catalogue entry");
    assert_clean(entry["summary"].as_str().unwrap(), "the summary");
    assert_clean(entry["remedy"].as_str().unwrap(), "the remedy");
}
