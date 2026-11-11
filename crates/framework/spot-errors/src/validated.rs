//! Loads and validates catalogue files: the rules every `catalogue/*.toml` must satisfy.
//!
//! Validation runs on load (so a broken compiled-in file surfaces through
//! [`Catalogue::load_error`](crate::Catalogue::load_error)) and in the test suite, which proves
//! the class names against the real `ErrorClass`. Rules:
//!
//! 1. TOML parses; every `[[entry]]` carries all required fields.
//! 2. `code` is dotted lower-case (`spot_app_api::error::is_code`).
//! 3. Codes are unique across all files.
//! 4. A file's name is its entries' prefix (`document.toml` holds `document.*`) — except
//!    `wire-vocabulary.toml`, the one file of collected non-error vocabulary.
//! 5. `class` names a real `ErrorClass` variant or is `none`.
//! 6. `summary` braces exactly every `params` name; `summary` and `remedy` are single
//!    sentences without stray whitespace.
//! 7. `explanation_key` is `error.` + the code; `params` are sorted without repeats; `since`
//!    is `MAJOR.MINOR.PATCH`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;

use crate::Entry;

/// The one file of collected non-error vocabulary (wire names, test site names); every other
/// file's name is its entries' prefix.
const VOCABULARY_FILE: &str = "wire-vocabulary.toml";

/// The `ErrorClass` variant names, as the catalogue's `class` field spells them. The test
/// suite proves independently that each name constructs a real variant.
fn class_names() -> Vec<&'static str> {
    vec![
        "Validation",
        "NotFound",
        "UnknownDocType",
        "StaleVersion",
        "Duplicate",
        "Linked",
        "InvalidState",
        "PermissionDenied",
        "MethodNotFound",
        "Unavailable",
        "Internal",
    ]
}

/// One raw `[[entry]]` row, as TOML carries it.
#[derive(Deserialize)]
struct RawEntry {
    code: String,
    class: String,
    summary: String,
    explanation_key: String,
    params: Vec<String>,
    remedy: String,
    since: String,
    #[serde(default)]
    retired: bool,
}

/// A whole parsed file.
#[derive(Deserialize)]
struct RawCatalogue {
    #[serde(default)]
    entry: Vec<RawEntry>,
}

/// Loads the files, collecting every rule violation into one error string (one line per
/// problem) or returning the validated entries.
pub(crate) fn validate(files: &[(&'_ str, &str)]) -> Result<crate::Loaded, String> {
    let mut errors: Vec<String> = Vec::new();
    let mut seen: BTreeMap<String, (&str, usize)> = BTreeMap::new();
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let classes = class_names();

    for (file, text) in files {
        order.push((*file).to_owned());
        let raw = match toml::from_str::<RawCatalogue>(text) {
            Ok(raw) => raw,
            Err(e) => {
                errors.push(format!("{file}: does not parse: {e}"));
                continue;
            }
        };
        if raw.entry.is_empty() {
            errors.push(format!("{file}: holds no [[entry]] rows"));
        }
        for (i, raw) in raw.entry.iter().enumerate() {
            let ordinal = i + 1;
            if !prefix_ok(file, &raw.code) {
                errors.push(format!(
                    "{file}: entry {ordinal} code {} is not under the file's prefix",
                    raw.code
                ));
            }
            if !spot_app_api::error::is_code(&raw.code) {
                errors.push(format!(
                    "{file}: entry {ordinal} code {} is not dotted lower-case",
                    raw.code
                ));
            }
            if let Some((first_file, first_ordinal)) = seen.get(&raw.code) {
                errors.push(format!(
                    "{file}: entry {ordinal} duplicates {first_file}: entry {first_ordinal} ({})",
                    raw.code
                ));
                continue;
            }
            seen.insert(raw.code.clone(), (file, ordinal));

            if raw.class != "none" && !classes.contains(&raw.class.as_str()) {
                errors.push(format!(
                    "{file}: entry {ordinal} ({}) class {} is no ErrorClass name and not `none`",
                    raw.code, raw.class
                ));
            }
            let braced = braces(&raw.summary);
            let declared: BTreeSet<&str> = raw.params.iter().map(String::as_str).collect();
            for name in &braced {
                if !declared.contains(name.as_str()) {
                    errors.push(format!(
                        "{file}: entry {ordinal} ({}) summary braces {name} but params is {:?}",
                        raw.code, raw.params
                    ));
                }
            }
            for name in &raw.params {
                if !braced.contains(name) {
                    errors.push(format!(
                        "{file}: entry {ordinal} ({}) declares param {name} but the summary never braces it",
                        raw.code
                    ));
                }
            }
            let mut sorted = raw.params.clone();
            sorted.sort();
            sorted.dedup();
            if sorted != raw.params {
                errors.push(format!(
                    "{file}: entry {ordinal} ({}) params are not sorted without repeats: {:?}",
                    raw.code, raw.params
                ));
            }
            if stray_whitespace(&raw.summary) {
                errors.push(format!(
                    "{file}: entry {ordinal} ({}) summary has stray whitespace",
                    raw.code
                ));
            }
            if stray_whitespace(&raw.remedy) {
                errors.push(format!(
                    "{file}: entry {ordinal} ({}) remedy has stray whitespace",
                    raw.code
                ));
            }
            let expected = format!("error.{}", raw.code);
            if raw.explanation_key != expected {
                errors.push(format!(
                    "{file}: entry {ordinal} ({}) explanation_key is {} but {expected} is the rule",
                    raw.code, raw.explanation_key
                ));
            }
            if !since_ok(&raw.since) {
                errors.push(format!(
                    "{file}: entry {ordinal} ({}) since {} is not MAJOR.MINOR.PATCH",
                    raw.code, raw.since
                ));
            }
            entries.insert(raw.code.clone(), build_entry(raw));
        }
    }

    if errors.is_empty() {
        Ok(crate::Loaded {
            entries,
            files: order,
        })
    } else {
        Err(errors.join("\n"))
    }
}

/// Whether `file` is the vocabulary file (exempt from the prefix rule).
fn is_vocabulary(file: &str) -> bool {
    file == VOCABULARY_FILE
}

/// Whether `code` belongs in `file`: the file name is the code's first segment (except in the
/// vocabulary file).
fn prefix_ok(file: &str, code: &str) -> bool {
    if is_vocabulary(file) {
        return true;
    }
    match file.strip_suffix(".toml") {
        Some(prefix) => code.starts_with(prefix) && code[prefix.len()..].starts_with('.'),
        None => false,
    }
}

/// Whether a sentence carries stray whitespace (double spaces, leading or trailing).
fn stray_whitespace(text: &str) -> bool {
    text.trim() != text || text.contains("  ")
}

/// The `{...}` parameter names a summary braces, in order.
fn braces(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = text[i + 1..].find('}')
        {
            names.push(text[i + 1..i + 1 + end].to_owned());
            i = i + 1 + end;
            continue;
        }
        i += 1;
    }
    names
}

/// Whether `since` is `MAJOR.MINOR.PATCH`.
fn since_ok(since: &str) -> bool {
    let parts: Vec<&str> = since.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Builds the public [`Entry`] from the raw row, with sorted, repeated-free params.
fn build_entry(raw: &RawEntry) -> Entry {
    let mut params = raw.params.clone();
    params.sort();
    params.dedup();
    Entry {
        code: raw.code.clone(),
        class: raw.class.clone(),
        summary: raw.summary.clone(),
        explanation_key: raw.explanation_key.clone(),
        params,
        remedy: raw.remedy.clone(),
        since: raw.since.clone(),
        retired: raw.retired,
    }
}

/// The explanation key rule, exercised by the test suite.
#[cfg(test)]
mod tests {
    use super::{braces, since_ok, stray_whitespace};

    #[test]
    fn braces_extracts_in_order() {
        assert_eq!(braces("No {a} and {b} here."), vec!["a", "b"]);
        assert_eq!(braces("None here."), Vec::<String>::new());
        assert_eq!(braces("one {only}"), vec!["only"]);
    }

    #[test]
    fn explanation_key_rule_holds_on_every_compiled_entry() {
        for (code, key) in [
            ("document.required", "error.document.required"),
            (
                "licence.clock.rewind_beyond_tolerance",
                "error.licence.clock.rewind_beyond_tolerance",
            ),
        ] {
            assert_eq!(format!("error.{code}"), key);
        }
    }

    #[test]
    fn since_checks_shape() {
        assert!(since_ok("0.1.0"));
        assert!(since_ok("1.20.3"));
        assert!(!since_ok("0.1"));
        assert!(!since_ok("0.1.x"));
        assert!(!since_ok(""));
    }

    #[test]
    fn stray_whitespace_checks() {
        assert!(stray_whitespace(" x"));
        assert!(stray_whitespace("x "));
        assert!(stray_whitespace("a  b"));
        assert!(!stray_whitespace("a b"));
    }
}
