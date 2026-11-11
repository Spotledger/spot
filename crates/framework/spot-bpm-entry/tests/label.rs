//! The run label pair: "Request" by default, overridable per pack, locale and organisation, and
//! the internal word never reaches a user.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use spot_bpm_entry::{
    EmptyCatalogue, EntryError, MemoryCatalogue, RunLabel, RunLabels, user_visible_strings,
};
use spot_bpm_entry::{FormNodeConfig, codes};

/// The internal word for a run, assembled so this file does not contain it as one literal.
fn internal_word() -> String {
    ["Ca", "se"].concat()
}

fn labels(catalogue: MemoryCatalogue) -> RunLabels {
    RunLabels::new(catalogue.shared())
}

#[test]
fn the_default_is_request_and_requests() {
    let resolver = RunLabels::new(std::sync::Arc::new(EmptyCatalogue));
    let label = resolver.label("en", None);
    assert_eq!(label.singular, "Request");
    assert_eq!(label.plural, "Requests");
    assert_eq!(label, RunLabel::shipped());
    // The catalogue keys are the ADR-080 pair.
    assert_eq!(RunLabels::SINGULAR_KEY, ["bpm.run", ".singular"].concat());
    assert_eq!(RunLabels::PLURAL_KEY, ["bpm.run", ".plural"].concat());
}

#[test]
fn a_pack_or_an_organisation_overrides_the_label() {
    let catalogue = MemoryCatalogue::new();
    // A school pack names runs "Application" for everyone in English.
    catalogue.set(RunLabels::SINGULAR_KEY, "en", None, "Application");
    catalogue.set(RunLabels::PLURAL_KEY, "en", None, "Applications");
    // One organisation says "Admission".
    catalogue.set(
        RunLabels::SINGULAR_KEY,
        "en",
        Some("north_school"),
        "Admission",
    );
    catalogue.set(
        RunLabels::PLURAL_KEY,
        "en",
        Some("north_school"),
        "Admissions",
    );
    let resolver = labels(catalogue);

    let pack = resolver.label("en", Some("south_school"));
    assert_eq!(
        (pack.singular.as_str(), pack.plural.as_str()),
        ("Application", "Applications")
    );
    let org = resolver.label("en", Some("north_school"));
    assert_eq!(
        (org.singular.as_str(), org.plural.as_str()),
        ("Admission", "Admissions")
    );
    // Another locale has no text, so the shipped default applies.
    assert_eq!(resolver.label("ur", None), RunLabel::shipped());

    // The strings every template builds follow the override.
    assert_eq!(org.start_action(), "Start a Admission");
    assert_eq!(org.my_heading(), "My Admissions");
}

#[test]
fn a_blank_catalogue_text_never_produces_an_empty_name() {
    let catalogue = MemoryCatalogue::new();
    catalogue.set(RunLabels::SINGULAR_KEY, "en", None, "   ");
    let label = labels(catalogue).label("en", None);
    assert_eq!(label.singular, "Request");
    assert!(matches!(
        RunLabel::with_overrides(Some(" "), None),
        Err(EntryError::LabelInvalid { .. })
    ));
    assert_eq!(
        RunLabel::with_overrides(Some("Claim"), Some("Claims"))
            .unwrap()
            .plural,
        "Claims"
    );
}

#[test]
fn start_replaces_new_and_the_waiting_line_names_the_request() {
    let label = RunLabel::shipped();
    assert_eq!(label.start_action(), "Start a Request");
    assert_eq!(label.my_heading(), "My Requests");
    assert_eq!(
        label.waiting_line("123"),
        "Request 123 is waiting for approval"
    );
}

#[test]
fn no_user_visible_string_names_a_run_with_the_internal_word() {
    let word = internal_word().to_lowercase();
    let overridden = RunLabel::with_overrides(Some("Claim"), Some("Claims")).unwrap();
    for label in [RunLabel::shipped(), overridden] {
        for text in user_visible_strings(&label) {
            assert!(
                !text
                    .to_lowercase()
                    .split_whitespace()
                    .any(|part| part == word),
                "`{text}` shows the internal word"
            );
        }
    }
    // Every error message a person can read is free of it too.
    let errors = [
        EntryError::OwnedByRequest {
            doctype: "employee".into(),
            request: "r1".into(),
        },
        EntryError::StartProcess {
            doctype: "employee".into(),
            process: "entry_employee".into(),
        },
        EntryError::NoAssignee {
            node: "enter".into(),
        },
        EntryError::StepNotOpen {
            task: "t1".into(),
            detail: "gone".into(),
        },
        EntryError::BatchState {
            run: "r1".into(),
            detail: "done".into(),
        },
        EntryError::FormInvalid { fields: Vec::new() },
    ];
    for error in errors {
        let text = error.to_string().to_lowercase();
        assert!(!text.split_whitespace().any(|part| part == word), "{text}");
    }
}

/// The crate's own source never writes the internal word inside a string literal: scans every
/// `src` file for a quoted run of text that contains it.
#[test]
fn the_crate_source_has_no_quoted_internal_word() {
    let word = internal_word();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let source = std::fs::read_to_string(&path).unwrap();
        for (number, line) in source.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            let mut quoted = line.split('"').skip(1).step_by(2);
            if quoted.any(|text| text.contains(&word)) {
                found.push(format!("{}:{}", path.display(), number + 1));
            }
        }
    }
    assert!(found.is_empty(), "the internal word is quoted at {found:?}");
}

#[test]
fn the_form_title_comes_from_the_label() {
    let label = RunLabel::with_overrides(Some("Claim"), Some("Claims")).unwrap();
    assert_eq!(label.form_title(), "Enter the details for this Claim");
    // Keeps the config type and codes in the public API surface.
    assert_eq!(codes::START_PROCESS, "bpm_entry.start_process");
    let config = serde_json::json!({"record_type": "employee", "draft_mode": "create"});
    assert!(FormNodeConfig::parse(&config, "enter").is_ok());
}
