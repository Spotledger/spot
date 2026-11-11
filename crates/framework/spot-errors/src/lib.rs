//! The error and message catalogue (WP-DOC-08.T1, ADR-26, ADR-055).
//!
//! Every stable dotted code the platform (and its test suites) can raise has one entry here:
//! its `ErrorClass`, a one-sentence developer-facing summary, the localisation key of its
//! longer explanation, the parameter names its message uses, a one-sentence remedy, and the
//! version that introduced it. The catalogue is compiled in from `catalogue/*.toml` with
//! [`include_str!`] and validated on load; a code that is raised anywhere in the repository
//! but has no entry fails the error-code completeness scan (`tools/lints/error-codes/`), not a
//! customer request.
//!
//! # What hosts do with it
//!
//! [`Catalogue::explain`] turns an [`AppError`] into an [`Explained`] value: code, summary in
//! the requested language, remedy, and the parameters the error carries. Languages fall back
//! to English; translations are added later, by explanation key.
//! [`Catalogue::missing_explanation`] tells the engine whether an error carries a code the
//! catalogue does not know — assert it is `false` in debug builds.
//!
//! # Rules the catalogue enforces
//!
//! - Codes are dotted lower-case (`document.required`), per `spot_app_api::error::is_code`.
//! - `class` names an `ErrorClass` variant, or `none` for codes raised as wire fields or
//!   library-internal vocabularies that never reach `AppError`'s class.
//! - Every parameter a summary braces must be listed in `params`; a message never names a
//!   parameter the catalogue does not declare.
//! - An error message never contains a secret or a raw SQL string (see `README.md`).
//!
//! ```no_run
//! use spot_app_api::{AppError, ErrorClass};
//! use spot_errors::Catalogue;
//!
//! let catalogue = Catalogue::load();
//! let err = AppError::new(ErrorClass::NotFound, "document.not_found", "No document with this id exists.");
//! let explained = catalogue.explain(&err, "de");
//! assert_eq!(explained.code, "document.not_found");
//! assert_eq!(explained.summary, "No document with this id exists, or the caller may not see it.");
//! assert!(!catalogue.missing_explanation(&err));
//! ```

use std::collections::BTreeMap;
use std::sync::OnceLock;

use spot_app_api::AppError;

mod validated;

#[cfg(test)]
mod tests;

/// One catalogue entry, as the `.toml` files carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The stable dotted code, e.g. `document.required`.
    pub code: String,
    /// The `ErrorClass` name (`"Validation"`, `"NotFound"`, ...), or `"none"` for
    /// library-internal vocabularies, wire-only registry codes and non-fatal message codes.
    pub class: String,
    /// One-sentence developer-facing summary. Braces name the parameters.
    pub summary: String,
    /// Localisation key of the longer explanation, e.g. `error.document.required`.
    pub explanation_key: String,
    /// Parameter names the message uses, sorted without repeats.
    pub params: Vec<String>,
    /// What a user or admin can do, one sentence.
    pub remedy: String,
    /// The platform version that introduced the code.
    pub since: String,
    /// A retired code no raise site produces any more; kept so old clients' codes still
    /// resolve.
    pub retired: bool,
}

/// The explanation of an [`AppError`] the catalogue produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explained {
    /// The error's code (the entry it resolved to).
    pub code: String,
    /// The entry's summary in the requested language (English fallback).
    pub summary: String,
    /// The entry's remedy in the requested language (English fallback).
    pub remedy: String,
    /// The parameters the error carries, by name.
    pub params: BTreeMap<String, String>,
}

/// What loading a set of catalogue files produced.
#[derive(Debug)]
pub struct Loaded {
    /// The validated entries, keyed by code.
    pub entries: BTreeMap<String, Entry>,
    /// The files in load order, for diagnostics.
    pub files: Vec<String>,
}

/// Every compiled-in catalogue file, in file name order: `include_str!` puts them in the binary.
/// The list is written by `build.rs` from `catalogue/*.toml`, so a new file is picked up without
/// touching this one.
pub(crate) static CATALOGUE_FILES: &[(&str, &str)] =
    include!(concat!(env!("OUT_DIR"), "/catalogue_files.rs"));

/// The compiled-in catalogue, validated once, shared for the process lifetime.
#[derive(Debug)]
pub struct Catalogue {
    entries: BTreeMap<String, Entry>,
    load_error: Option<String>,
}

impl Catalogue {
    /// Loads and validates the compiled-in catalogue.
    ///
    /// The compiled-in files are proven valid by the test suite, so the returned catalogue is
    /// complete; a future file that breaks a rule surfaces through [`Catalogue::load_error`]
    /// and an empty entry map, which the engine's `missing_explanation` debug assertions turn
    /// loud — never a wrong answer served quietly.
    #[must_use]
    pub fn load() -> &'static Self {
        static LOADED: OnceLock<Catalogue> = OnceLock::new();
        LOADED.get_or_init(|| match Self::load_files(CATALOGUE_FILES) {
            Ok(loaded) => Catalogue {
                entries: loaded.entries,
                load_error: None,
            },
            Err(errors) => Catalogue {
                entries: BTreeMap::new(),
                load_error: Some(errors),
            },
        })
    }

    /// Loads and validates file contents (the seam the test suite drives).
    ///
    /// Returns `Err` with one line per problem when the files break a rule: duplicate codes,
    /// invalid dotted form, unknown class names, summary braces that `params` does not list,
    /// or a file whose name is not its entries' prefix.
    pub fn load_files(files: &[(&str, &str)]) -> Result<Loaded, String> {
        validated::validate(files)
    }

    /// The entry for `code`, when the catalogue knows it.
    #[must_use]
    pub fn lookup(&self, code: &str) -> Option<&Entry> {
        self.entries.get(code)
    }

    /// Every entry, keyed by code.
    #[must_use]
    pub fn entries(&self) -> &BTreeMap<String, Entry> {
        &self.entries
    }

    /// Why the compiled-in files failed validation, if they did. `None` in every build the
    /// test suite passes.
    #[must_use]
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Explains `error` in `language`, falling back to English when the language has no
    /// translation for the entry's explanation key (translations are added later, by key).
    ///
    /// Parameters come from the error's field errors and details, by name. For a code the
    /// catalogue does not know, the explanation is the error's own message.
    #[must_use]
    pub fn explain(&self, error: &AppError, language: &str) -> Explained {
        let code = error.code.clone();
        let Some(entry) = self.entries.get(&code) else {
            return Explained {
                code,
                summary: error.message.clone(),
                remedy: String::new(),
                params: collect_params(error),
            };
        };
        let (summary, remedy) = match translation(entry, language) {
            Some((s, r)) => (s, r),
            None => (entry.summary.clone(), entry.remedy.clone()),
        };
        Explained {
            code,
            summary,
            remedy,
            params: collect_params(error),
        }
    }

    /// Whether `error` carries a code the catalogue does not know. The engine asserts this is
    /// `false` in debug builds.
    #[must_use]
    pub fn missing_explanation(&self, error: &AppError) -> bool {
        !self.entries.contains_key(error.code.as_str())
    }
}

/// The translated `summary` and `remedy` of `entry` in `language`, when one exists.
///
/// No translation tables are compiled in yet (WP-DOC-08 keeps the keys stable; translations
/// land later, keyed by the entry's `explanation_key`). Every language therefore falls back to
/// English, and callers can always ask for their language.
fn translation(entry: &Entry, language: &str) -> Option<(String, String)> {
    let _ = (language, &entry.explanation_key);
    None
}

/// Collects an error's parameters from its field errors and details, by name. The first field
/// error fills `field`; details fill their own names and never override it.
fn collect_params(error: &AppError) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    if let Some(first) = error.fields.first() {
        params.insert("field".to_owned(), first.field.to_string());
    }
    for (key, value) in &error.details {
        params
            .entry(key.clone())
            .or_insert_with(|| value.to_string());
    }
    params
}
