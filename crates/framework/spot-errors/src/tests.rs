//! The catalogue's test suite: the compiled-in files satisfy every rule, and explain behaves.

use std::collections::BTreeSet;

use proptest::prelude::*;
use spot_app_api::{AppError, ErrorClass, FieldPath};

use crate::Catalogue;

/// The compiled-in files, from the crate root's include table.
fn compiled() -> Vec<(&'static str, &'static str)> {
    crate::CATALOGUE_FILES
        .iter()
        .map(|(name, text)| (name.to_owned(), *text))
        .collect()
}

/// The `ErrorClass` variant names, and the registry code each is reported as — the proof that
/// the catalogue's `class` names are real variants of the contract's enum.
fn real_class_names() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Validation", "document.validation_failed"),
        ("NotFound", "document.not_found"),
        ("UnknownDocType", "meta.unknown_doctype"),
        ("StaleVersion", "document.stale_version"),
        ("Duplicate", "document.duplicate"),
        ("Linked", "document.linked"),
        ("InvalidState", "document.invalid_state"),
        ("PermissionDenied", "permission.denied"),
        ("MethodNotFound", "method.not_found"),
        ("Unavailable", "server.unavailable"),
        ("Internal", "http.internal"),
    ]
}

/// Loading the real files succeeds and yields non-empty entries.
#[test]
fn compiled_catalogue_loads_clean() {
    let loaded = match Catalogue::load_files(&compiled()) {
        Ok(loaded) => loaded,
        Err(errors) => panic!("the compiled-in catalogue breaks a rule:\n{errors}"),
    };
    assert!(!loaded.entries.is_empty(), "the catalogue holds no entries");
    assert_eq!(
        loaded.files.len(),
        compiled().len(),
        "every file was walked"
    );
}

/// The compiled `Catalogue::load()` singleton has no load error and resolves a known code.
#[test]
fn load_singleton_is_valid() {
    let catalogue = Catalogue::load();
    assert_eq!(catalogue.load_error(), None, "compiled files must validate");
    let entry = catalogue
        .lookup("document.required")
        .unwrap_or_else(|| panic!("document.required is catalogued"));
    assert_eq!(entry.class, "Validation");
    assert_eq!(entry.explanation_key, "error.document.required");
    assert_eq!(entry.params, vec!["field".to_owned()]);
}

/// Every compiled entry's `class` names a real `ErrorClass` variant or is `none` — proved
/// against the names the contract's own enum spells, not a copy that can drift.
#[test]
fn every_class_name_is_a_real_variant_or_none() {
    let real: BTreeSet<&str> = real_class_names()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let catalogue = Catalogue::load();
    for (code, entry) in catalogue.entries() {
        if entry.class == "none" {
            continue;
        }
        assert!(
            real.contains(entry.class.as_str()),
            "{code} names class {} which no ErrorClass variant spells",
            entry.class
        );
    }
}

/// Each real class name maps to the registry code the contract documents — the other half of
/// the proof (a renamed variant would change this table's source, not pass silently).
#[test]
fn class_names_map_to_registry_codes() {
    for (name, api_code) in real_class_names() {
        let class = class_by_name(name).unwrap_or_else(|| panic!("{name} constructs"));
        assert_eq!(class.api_code(), api_code, "{name} maps to {api_code}");
    }
}

/// Constructs the real `ErrorClass` for a name.
fn class_by_name(name: &str) -> Option<ErrorClass> {
    match name {
        "Validation" => Some(ErrorClass::Validation),
        "NotFound" => Some(ErrorClass::NotFound),
        "UnknownDocType" => Some(ErrorClass::UnknownDocType),
        "StaleVersion" => Some(ErrorClass::StaleVersion),
        "Duplicate" => Some(ErrorClass::Duplicate),
        "Linked" => Some(ErrorClass::Linked),
        "InvalidState" => Some(ErrorClass::InvalidState),
        "PermissionDenied" => Some(ErrorClass::PermissionDenied),
        "MethodNotFound" => Some(ErrorClass::MethodNotFound),
        "Unavailable" => Some(ErrorClass::Unavailable),
        "Internal" => Some(ErrorClass::Internal),
        _ => None,
    }
}

/// Field errors feed the `field` parameter; details feed their named parameters.
#[test]
fn explain_fills_params_from_fields_and_details() {
    let catalogue = Catalogue::load();
    let error = AppError::field(
        FieldPath::row("items", 2, "qty"),
        "document.invalid_value",
        "The quantity is not a number.",
    )
    .detail("current_version", 7u64);
    let explained = catalogue.explain(&error, "en");
    assert_eq!(explained.code, "document.validation_failed");
    assert_eq!(
        explained.params.get("field"),
        Some(&"items[2].qty".to_owned()),
        "the first field error fills the field param"
    );
    assert_eq!(
        explained.params.get("current_version"),
        Some(&"7".to_owned())
    );
}

/// The summary braces exactly every declared param, on every compiled entry — the rule the
/// validator checks, proved directly against the compiled rows.
#[test]
fn every_summary_braces_exactly_its_params() {
    let catalogue = Catalogue::load();
    for (code, entry) in catalogue.entries() {
        let braced: BTreeSet<String> = braced_names(&entry.summary);
        let declared: BTreeSet<&str> = entry.params.iter().map(String::as_str).collect();
        let braced_ref: BTreeSet<&str> = braced.iter().map(String::as_str).collect();
        assert_eq!(
            braced_ref, declared,
            "{code}: summary braces {braced:?} but declares {:?}",
            entry.params
        );
    }
}

/// The `{...}` names a summary braces.
fn braced_names(summary: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let bytes = summary.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = summary[i + 1..].find('}')
        {
            names.insert(summary[i + 1..i + 1 + end].to_owned());
            i = i + 1 + end;
            continue;
        }
        i += 1;
    }
    names
}

/// An unknown code looks up to `None`, and `missing_explanation` reports it.
#[test]
fn unknown_codes_lookup_to_none() {
    let catalogue = Catalogue::load();
    let error = AppError::new(ErrorClass::Internal, "app1.qty_not_positive", "boom");
    assert!(catalogue.lookup("app1.qty_not_positive").is_none());
    assert!(catalogue.missing_explanation(&error));
    let known = AppError::new(ErrorClass::NotFound, "document.not_found", "gone");
    assert!(!catalogue.missing_explanation(&known));
}

/// `explain` of an unknown code falls back to the error's own message, with no remedy.
#[test]
fn unknown_code_falls_back_to_the_error_message() {
    let catalogue = Catalogue::load();
    let error = AppError::new(
        ErrorClass::Internal,
        "app1.qty_not_positive",
        "The quantity is not positive.",
    );
    let explained = catalogue.explain(&error, "en");
    assert_eq!(explained.code, "app1.qty_not_positive");
    assert_eq!(explained.summary, "The quantity is not positive.");
    assert_eq!(explained.remedy, "");
}

/// A language the catalogue has no table for falls back to English.
#[test]
fn language_falls_back_to_english() {
    let catalogue = Catalogue::load();
    let error = AppError::new(
        ErrorClass::NotFound,
        "document.not_found",
        "No document with this id exists.",
    );
    let english = catalogue.explain(&error, "en");
    let german = catalogue.explain(&error, "de");
    assert_eq!(english, german, "no translation table is compiled in yet");
    assert_eq!(
        german.summary,
        "No document with this id exists, or the caller may not see it."
    );
    assert_eq!(
        german.remedy,
        "Check the id and the caller's read permission."
    );
}

/// The validator rejects: duplicate codes, invalid form, unknown class, unbraced params,
/// undeclared braces, wrong file prefix, wrong explanation key, bad since, unparsable TOML,
/// unsorted params. Each break is caught on a synthetic file.
#[test]
fn validator_rejects_broken_files() {
    let clean = |code: &str, class: &str, summary: &str, params: &str| -> String {
        format!(
            r#"
[[entry]]
code = "{code}"
class = "{class}"
summary = "{summary}"
explanation_key = "error.{code}"
params = [{params}]
remedy = "Fix it."
since = "0.1.0"
"#
        )
    };
    let broken: Vec<(&str, String)> = vec![
        (
            "duplicate code",
            clean(
                "document.required",
                "Validation",
                "First: {field}.",
                "\"field\"",
            ) + &clean("document.required", "Validation", "Second.", ""),
        ),
        (
            "invalid dotted form",
            clean(
                "Document.Required",
                "Validation",
                "Upper case is refused.",
                "",
            ),
        ),
        (
            "unknown class",
            clean(
                "document.required",
                "Catastrophic",
                "No such ErrorClass.",
                "",
            ),
        ),
        (
            "unbraced declared param",
            clean(
                "document.required",
                "Validation",
                "Nothing is braced here.",
                "\"field\"",
            ),
        ),
        (
            "undeclared brace",
            clean(
                "document.required",
                "Validation",
                "The {mystery} param is not declared.",
                "",
            ),
        ),
        // A file named for another prefix must not hold document codes; proved separately in
        // `validator_rejects_wrong_file_prefix`.
        ("wrong explanation key", {
            let e = clean("document.required", "Validation", "Fine.", "");
            e.replace(
                "explanation_key = \"error.document.required\"",
                "explanation_key = \"errors.document.required\"",
            )
        }),
        ("bad since", {
            let mut e = clean("document.required", "Validation", "Fine.", "");
            e = e.replace("since = \"0.1.0\"", "since = \"not-a-version\"");
            e
        }),
        ("unsorted params", {
            let mut e = clean(
                "document.required",
                "Validation",
                "Needs {a} and {b}.",
                "\"b\", \"a\"",
            );
            e = e.replace("explanation_key = ", "unused_key = ");
            e.replace(
                "unused_key = \"error.document.required\"",
                "explanation_key = \"error.document.required\"",
            )
        }),
        ("unparsable toml", "code = [broken".to_owned()),
    ];
    for (name, text) in broken {
        let leaked: &'static str = Box::leak(text.into_boxed_str());
        let file: &'static str = match name {
            "wrong file prefix" => "wrong.toml",
            _ => "document.toml",
        };
        let files = vec![(file, leaked)];
        let result = Catalogue::load_files(&files);
        assert!(result.is_err(), "{name}: validator accepted a broken file");
    }
}

/// The validator accepts a clean synthetic file with multiple params in order.
#[test]
fn validator_accepts_a_clean_synthetic_file() {
    let text: &'static str = Box::leak(
        r#"
[[entry]]
code = "document.required"
class = "Validation"
summary = "A required field has no value: {field}."
explanation_key = "error.document.required"
params = ["field"]
remedy = "Fill the field."
since = "0.1.0"

[[entry]]
code = "document.too_long"
class = "Validation"
summary = "The text is too long for {field} by {n} chars."
explanation_key = "error.document.too_long"
params = ["field", "n"]
remedy = "Shorten the text."
since = "0.1.0"
"#
        .to_owned()
        .into_boxed_str(),
    );
    let files = vec![("document.toml", text)];
    let loaded =
        Catalogue::load_files(&files).unwrap_or_else(|e| panic!("clean file validates: {e}"));
    assert_eq!(loaded.entries.len(), 2);
    let entry = loaded
        .entries
        .get("document.too_long")
        .unwrap_or_else(|| panic!("document.too_long present"));
    assert_eq!(entry.params, vec!["field".to_owned(), "n".to_owned()]);
    assert!(!entry.retired);
    assert!(!loaded.files.is_empty());
}

/// The wrong-file-prefix rule: a `wrong.toml` must not hold `document.*` codes.
#[test]
fn validator_rejects_wrong_file_prefix() {
    let text: &'static str = Box::leak(
        r#"
[[entry]]
code = "document.required"
class = "Validation"
summary = "A required field has no value."
explanation_key = "error.document.required"
params = []
remedy = "Fill the field."
since = "0.1.0"
"#
        .to_owned()
        .into_boxed_str(),
    );
    let files = vec![("wrong.toml", text)];
    let result = Catalogue::load_files(&files);
    let message = match result {
        Err(message) => message,
        Ok(loaded) => panic!(
            "wrong.toml should not hold document codes, got {:?}",
            loaded.entries
        ),
    };
    assert!(
        message.contains("not under the file's prefix"),
        "rejection names the rule: {message}"
    );
}

/// A retired entry stays resolvable and carries the flag.
#[test]
fn retired_entries_stay_resolvable() {
    let text: &'static str = Box::leak(
        r#"
[[entry]]
code = "document.old_code"
class = "Validation"
summary = "An old code kept for old clients."
explanation_key = "error.document.old_code"
params = []
remedy = "Use the new code."
since = "0.1.0"
retired = true
"#
        .to_owned()
        .into_boxed_str(),
    );
    let files = vec![("document.toml", text)];
    let loaded =
        Catalogue::load_files(&files).unwrap_or_else(|e| panic!("retired entry validates: {e}"));
    let entry = loaded
        .entries
        .get("document.old_code")
        .unwrap_or_else(|| panic!("retired entry present"));
    assert!(entry.retired);
}

/// Every entry's `since` seeds at the workspace version.
#[test]
fn since_is_the_workspace_version() {
    let catalogue = Catalogue::load();
    for (code, entry) in catalogue.entries() {
        assert_eq!(
            entry.since, "0.1.0",
            "{code}: seeded at the workspace version"
        );
    }
}

/// The vocabulary file's entries carry classes of `none` (they are not raised as AppError).
#[test]
fn vocabulary_entries_are_class_none() {
    let catalogue = Catalogue::load();
    for (code, entry) in catalogue.entries() {
        if code.starts_with("capabilities.") || code.starts_with("service.") {
            assert_eq!(
                entry.class, "none",
                "{code} is vocabulary, not a raised error"
            );
        }
    }
}

// Property: explaining arbitrary errors never panics and keeps the error's code.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn explain_never_panics(
        code in "[a-z][a-z0-9_]{0,8}(\\.[a-z][a-z0-9_]{0,8}){1,3}",
        message in "[^\"]{0,80}",
        language in prop::option::of("[a-z]{2,8}"),
    ) {
        let catalogue = Catalogue::load();
        let error = AppError::new(ErrorClass::Internal, &code, message.clone());
        let explained = catalogue.explain(&error, language.as_deref().unwrap_or("en"));
        prop_assert_eq!(explained.code, code.clone());
        if catalogue.lookup(&code).is_none() {
            prop_assert_eq!(explained.summary, message);
        }
    }
}
