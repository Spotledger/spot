//! The offline help catalogue: one [`Help`] record per error code, bundled into the binary.
//!
//! Help lives in `help/*.toml` beside this crate, one file per code prefix, as `[[help]]`
//! tables. `build.rs` compiles the files in with `include_str!`, so [`HelpIndex::load`] needs
//! no runtime files and works on an air-gapped machine. Lookup is locale-aware and falls back
//! from `de-CH` to `de` to the default locale (`en`), the same chain ADR-073 uses for the
//! locale catalogue.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::error::{SupportError, codes};

/// The locale every help entry falls back to.
pub const DEFAULT_LOCALE: &str = "en";

/// `(file name, contents)` of every `help/*.toml`, in file name order.
pub const HELP_FILES: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/help_files.rs"));

/// Who the help is written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Audience {
    /// Anyone using the product.
    User,
    /// The person who administers the site.
    Admin,
    /// Whoever runs the platform.
    Operator,
}

/// The plain-language help for one error code in one locale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Help {
    /// The error code this explains.
    pub code: String,
    /// The locale of the text; `en` when omitted in the file.
    #[serde(default = "default_locale")]
    pub locale: String,
    /// What happened, in one or two short sentences.
    pub what_happened: String,
    /// Why it usually happens.
    pub why: String,
    /// What to do, one step per item, in order.
    pub what_to_do: Vec<String>,
    /// A document id under `docs/` (path without the extension) with more detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learn_more: Option<String>,
    /// Who the text is for.
    pub audience: Audience,
}

fn default_locale() -> String {
    DEFAULT_LOCALE.to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelpFile {
    #[serde(default)]
    help: Vec<Help>,
}

/// Every help entry, indexed by code and locale.
#[derive(Clone, Debug, Default)]
pub struct HelpIndex {
    by_code: BTreeMap<String, BTreeMap<String, Help>>,
}

impl HelpIndex {
    /// The bundled help, parsed once and shared. A bundled file that fails to parse is a
    /// build defect the test suite catches; it surfaces here as an empty index.
    #[must_use]
    pub fn load() -> &'static Self {
        static LOADED: OnceLock<HelpIndex> = OnceLock::new();
        LOADED.get_or_init(|| Self::load_files(HELP_FILES).unwrap_or_default())
    }

    /// Parses help files (the seam the tests drive).
    ///
    /// # Errors
    /// One `help_malformed` error per file that does not parse and per duplicate
    /// `(code, locale)` pair.
    pub fn load_files(files: &[(&str, &str)]) -> Result<Self, Vec<SupportError>> {
        let mut index = Self::default();
        let mut errors = Vec::new();
        for (name, text) in files {
            match toml::from_str::<HelpFile>(text) {
                Ok(file) => {
                    for help in file.help {
                        let slot = index.by_code.entry(help.code.clone()).or_default();
                        if slot.contains_key(&help.locale) {
                            errors.push(SupportError::new(
                                codes::HELP_MALFORMED,
                                format!("{name}: a second entry for {}", help.code),
                            ));
                        } else {
                            slot.insert(help.locale.clone(), help);
                        }
                    }
                }
                Err(e) => errors.push(SupportError::new(
                    codes::HELP_MALFORMED,
                    format!("{name}: {e}"),
                )),
            }
        }
        if errors.is_empty() {
            Ok(index)
        } else {
            Err(errors)
        }
    }

    /// The help for `code` in `locale`, falling back through the shorter locale to English.
    #[must_use]
    pub fn lookup(&self, code: &str, locale: &str) -> Option<&Help> {
        let by_locale = self.by_code.get(code)?;
        locale_chain(locale)
            .iter()
            .find_map(|candidate| by_locale.get(candidate))
    }

    /// Whether any locale has help for `code`.
    #[must_use]
    pub fn contains(&self, code: &str) -> bool {
        self.by_code.contains_key(code)
    }

    /// Every code that has help, sorted.
    pub fn codes(&self) -> impl Iterator<Item = &str> {
        self.by_code.keys().map(String::as_str)
    }

    /// Every entry in every locale, sorted by code then locale.
    pub fn entries(&self) -> impl Iterator<Item = &Help> {
        self.by_code.values().flat_map(BTreeMap::values)
    }
}

/// The lookup order for `locale`: itself, its language part, then the default.
#[must_use]
pub fn locale_chain(locale: &str) -> Vec<String> {
    let normal = locale.trim().replace('_', "-");
    let mut chain = Vec::new();
    if !normal.is_empty() {
        chain.push(normal.clone());
        if let Some((language, _)) = normal.split_once('-') {
            chain.push(language.to_owned());
        }
    }
    if !chain.iter().any(|c| c == DEFAULT_LOCALE) {
        chain.push(DEFAULT_LOCALE.to_owned());
    }
    chain
}
