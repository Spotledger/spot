//! The run-label pair (ADR-080).
//!
//! User-visible text never says the internal word "Case". Every name for a run comes from the
//! label pair `bpm.run.singular` / `bpm.run.plural` — default "Request"/"Requests", overridable
//! per organisation and localised. The keys and the defaults live in
//! [`spot_bpm_model::model::labels`]; this crate re-exports them and carries the resolved pair
//! the report titles and column headings are built from.

use spot_bpm_model::model::labels::{
    RUN_PLURAL_DEFAULT, RUN_PLURAL_KEY, RUN_SINGULAR_DEFAULT, RUN_SINGULAR_KEY,
};

use crate::error::{InsightError, InsightResult};

/// The resolved run-label pair: the singular and plural names for one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunLabels {
    singular: String,
    plural: String,
}

impl RunLabels {
    /// The label key for one run (`bpm.run.singular`).
    pub const SINGULAR_KEY: &'static str = RUN_SINGULAR_KEY;
    /// The label key for several runs (`bpm.run.plural`).
    pub const PLURAL_KEY: &'static str = RUN_PLURAL_KEY;

    /// The default pair: "Request" / "Requests".
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            singular: RUN_SINGULAR_DEFAULT.to_owned(),
            plural: RUN_PLURAL_DEFAULT.to_owned(),
        }
    }

    /// Resolves the pair from an organisation's overrides, falling back to the defaults.
    ///
    /// # Errors
    /// [`InsightError::LabelInvalid`] when an override is present but empty; the default is used
    /// for a `None`.
    pub fn new(singular: Option<&str>, plural: Option<&str>) -> InsightResult<Self> {
        let singular = resolve(singular, "singular", RUN_SINGULAR_DEFAULT)?;
        let plural = resolve(plural, "plural", RUN_PLURAL_DEFAULT)?;
        Ok(Self { singular, plural })
    }

    /// The singular name for one run.
    #[must_use]
    pub fn singular(&self) -> &str {
        &self.singular
    }

    /// The plural name for several runs.
    #[must_use]
    pub fn plural(&self) -> &str {
        &self.plural
    }
}

impl Default for RunLabels {
    fn default() -> Self {
        Self::defaults()
    }
}

fn resolve(override_value: Option<&str>, which: &str, default: &str) -> InsightResult<String> {
    match override_value {
        None => Ok(default.to_owned()),
        Some(value) if value.trim().is_empty() => Err(InsightError::LabelInvalid {
            which: which.to_owned(),
        }),
        Some(value) => Ok(value.to_owned()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_request_and_requests() {
        let labels = RunLabels::defaults();
        assert_eq!(labels.singular(), "Request");
        assert_eq!(labels.plural(), "Requests");
        assert_eq!(RunLabels::SINGULAR_KEY, "bpm.run.singular");
        assert_eq!(RunLabels::PLURAL_KEY, "bpm.run.plural");
    }

    #[test]
    fn an_organisation_may_override_the_pair() {
        let labels = RunLabels::new(Some("Ticket"), Some("Tickets")).unwrap();
        assert_eq!(labels.singular(), "Ticket");
        assert_eq!(labels.plural(), "Tickets");
    }

    #[test]
    fn an_empty_override_is_refused() {
        assert!(matches!(
            RunLabels::new(Some("  "), None),
            Err(InsightError::LabelInvalid { .. })
        ));
        assert!(matches!(
            RunLabels::new(None, Some("")),
            Err(InsightError::LabelInvalid { .. })
        ));
    }
}
