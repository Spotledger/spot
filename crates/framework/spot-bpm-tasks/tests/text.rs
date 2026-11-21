//! ADR-080: user-visible text never names a run with the internal word.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_tasks::{RunLabel, user_visible_strings};

#[test]
fn user_visible_text_never_names_a_case() {
    let labels = [
        RunLabel::shipped(),
        RunLabel::with_overrides(Some("Application"), Some("Applications")),
        RunLabel::with_overrides(Some("Admission"), None),
        RunLabel::with_overrides(Some("  "), Some("")),
    ];
    for label in &labels {
        for string in user_visible_strings(label) {
            assert!(
                !string.contains("Case"),
                "user-visible string names a run with the internal word: {string}"
            );
        }
    }
}

#[test]
fn the_shipped_labels_are_request() {
    let shipped = RunLabel::shipped();
    assert_eq!(shipped.singular, "Request");
    assert_eq!(shipped.plural, "Requests");
    assert_eq!(RunLabel::default(), shipped);
}

#[test]
fn overrides_replace_but_blank_falls_back() {
    let overridden = RunLabel::with_overrides(Some("Claim"), Some("Claims"));
    assert_eq!(overridden.singular, "Claim");
    assert_eq!(overridden.plural, "Claims");

    let blank = RunLabel::with_overrides(Some("  "), None);
    assert_eq!(blank.singular, "Request");
    assert_eq!(blank.plural, "Requests");
}
