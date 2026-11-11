//! The user-facing name of a run (ADR-080 section 3).
//!
//! Users never see the internal word for a run. Every screen, notification and report takes the
//! name from the label pair `bpm.run.singular` / `bpm.run.plural` in the localisation catalogue
//! (ADR-073). The shipped default is "Request" / "Requests"; a pack or an organisation may
//! override either. The label is configuration data, not code.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use spot_bpm_model::model::labels::{
    RUN_PLURAL_DEFAULT, RUN_PLURAL_KEY, RUN_SINGULAR_DEFAULT, RUN_SINGULAR_KEY,
};

use crate::error::{EntryError, EntryResult};

/// The localisation catalogue, as far as this crate reads it: one text per key, locale and
/// (optionally) organisation.
pub trait LabelCatalogue: Send + Sync {
    /// The text for `key` in `locale`, with the organisation's override when `org` has one.
    /// `None` when the catalogue has no text, so the shipped default applies.
    fn text(&self, key: &str, locale: &str, org: Option<&str>) -> Option<String>;
}

/// A catalogue that has no texts: every label is the shipped default.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyCatalogue;

impl LabelCatalogue for EmptyCatalogue {
    fn text(&self, _key: &str, _locale: &str, _org: Option<&str>) -> Option<String> {
        None
    }
}

/// An in-memory catalogue for tests and single-site hosts.
#[derive(Debug, Default)]
pub struct MemoryCatalogue {
    // (key, locale, org) -> text; an empty org is the pack-wide text for the locale.
    texts: RwLock<BTreeMap<(String, String, String), String>>,
}

impl MemoryCatalogue {
    /// An empty catalogue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the text of `key` in `locale`, for one organisation or (with `None`) for everyone.
    pub fn set(&self, key: &str, locale: &str, org: Option<&str>, text: &str) {
        self.texts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                (
                    key.to_owned(),
                    locale.to_owned(),
                    org.unwrap_or_default().to_owned(),
                ),
                text.to_owned(),
            );
    }

    /// Wraps the catalogue for sharing.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl LabelCatalogue for MemoryCatalogue {
    fn text(&self, key: &str, locale: &str, org: Option<&str>) -> Option<String> {
        let texts = self
            .texts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lookup = |org: &str| {
            texts
                .get(&(key.to_owned(), locale.to_owned(), org.to_owned()))
                .cloned()
        };
        org.and_then(lookup).or_else(|| lookup(""))
    }
}

/// The user-facing name of a run: a singular and a plural.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunLabel {
    /// One run, for example "Request".
    pub singular: String,
    /// Several runs, for example "Requests".
    pub plural: String,
}

impl Default for RunLabel {
    fn default() -> Self {
        Self::shipped()
    }
}

impl RunLabel {
    /// The shipped default pair.
    #[must_use]
    pub fn shipped() -> Self {
        Self {
            singular: RUN_SINGULAR_DEFAULT.to_owned(),
            plural: RUN_PLURAL_DEFAULT.to_owned(),
        }
    }

    /// The pair with explicit overrides; a blank override is refused rather than shown as an
    /// empty name.
    ///
    /// # Errors
    /// [`EntryError::LabelInvalid`] when an override is present but blank.
    pub fn with_overrides(singular: Option<&str>, plural: Option<&str>) -> EntryResult<Self> {
        let shipped = Self::shipped();
        Ok(Self {
            singular: override_or(singular, "singular", shipped.singular)?,
            plural: override_or(plural, "plural", shipped.plural)?,
        })
    }

    /// The action text: "Start a Request" (replaces "New", ADR-080 section 2).
    #[must_use]
    pub fn start_action(&self) -> String {
        format!("Start a {}", self.singular)
    }

    /// The inbox heading: "My Requests".
    #[must_use]
    pub fn my_heading(&self) -> String {
        format!("My {}", self.plural)
    }

    /// The status line for a waiting run: "Request 123 is waiting for approval".
    #[must_use]
    pub fn waiting_line(&self, number: &str) -> String {
        format!("{} {number} is waiting for approval", self.singular)
    }

    /// The task title a form step shows: "Enter the details for this Request".
    #[must_use]
    pub fn form_title(&self) -> String {
        format!("Enter the details for this {}", self.singular)
    }

    /// The batch summary line: "5,000 records were started in one Request".
    #[must_use]
    pub fn batch_line(&self, records: usize) -> String {
        format!("{records} records belong to this {}", self.singular)
    }
}

fn override_or(value: Option<&str>, which: &str, default: String) -> EntryResult<String> {
    match value {
        None => Ok(default),
        Some(text) if text.trim().is_empty() => Err(EntryError::LabelInvalid {
            which: which.to_owned(),
        }),
        Some(text) => Ok(text.trim().to_owned()),
    }
}

/// Resolves the label pair from the localisation catalogue.
pub struct RunLabels {
    catalogue: Arc<dyn LabelCatalogue>,
}

impl RunLabels {
    /// A resolver over `catalogue`.
    #[must_use]
    pub fn new(catalogue: Arc<dyn LabelCatalogue>) -> Self {
        Self { catalogue }
    }

    /// The catalogue key of the singular label.
    pub const SINGULAR_KEY: &'static str = RUN_SINGULAR_KEY;
    /// The catalogue key of the plural label.
    pub const PLURAL_KEY: &'static str = RUN_PLURAL_KEY;

    /// The pair for `locale` and (optionally) an organisation: the organisation's text first,
    /// then the pack's text for the locale, then the shipped default. A blank text in the
    /// catalogue is ignored, so a bad setting never produces an empty name.
    #[must_use]
    pub fn label(&self, locale: &str, org: Option<&str>) -> RunLabel {
        let shipped = RunLabel::shipped();
        let pick = |key: &str, default: String| {
            self.catalogue
                .text(key, locale, org)
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
                .unwrap_or(default)
        };
        RunLabel {
            singular: pick(Self::SINGULAR_KEY, shipped.singular),
            plural: pick(Self::PLURAL_KEY, shipped.plural),
        }
    }
}

/// Every user-visible string this crate can produce for a label pair, so a test can scan them
/// for the internal word without driving the whole crate. Notification and report templates
/// call the same helpers.
#[must_use]
pub fn user_visible_strings(label: &RunLabel) -> Vec<String> {
    vec![
        label.start_action(),
        label.my_heading(),
        label.waiting_line("123"),
        label.form_title(),
        label.batch_line(5000),
        format!("{} by process", label.plural),
        format!("No {} to enter", label.plural),
    ]
}
