//! What the kit reports: a [`Violation`] names the operation, step, store and delta, and says
//! how to fix it; a [`Report`] is the list for a run.

use std::fmt;

use spot_app_api::{StepName, StoreName};

/// The stable codes of the kit's findings.
pub mod codes {
    /// After post and cancel a declared store does not net to its starting aggregate.
    pub const NOT_NETTED: &str = "effect_kit.not_netted_to_zero";
    /// Cancel reversed more than post wrote.
    pub const OVER_REVERSAL: &str = "effect_kit.over_reversal";
    /// A store row went away: rows are append-only.
    pub const ROW_DELETED: &str = "effect_kit.row_deleted";
    /// A store gained rows the operation's receipt does not report.
    pub const UNREPORTED_WRITE: &str = "effect_kit.unreported_write";
    /// The receipt reports more rows than the store gained.
    pub const OVER_REPORTED: &str = "effect_kit.over_reported";
    /// The receipt holds an effect no declaration covers.
    pub const UNDECLARED_REPORT: &str = "effect_kit.undeclared_report";
    /// The reversing rows do not reference the rows they reverse.
    pub const REVERSAL_UNLINKED: &str = "effect_kit.reversal_unlinked";
    /// A second cancel of the same document was accepted.
    pub const CANCEL_NOT_REFUSED: &str = "effect_kit.cancel_not_refused";
    /// A refused second cancel still changed a store.
    pub const CANCEL_DUPLICATED: &str = "effect_kit.cancel_duplicated";
    /// Correct did not cancel the original and return a linked new draft.
    pub const CORRECT_BROKEN: &str = "effect_kit.correct_not_cancel_plus_new";
    /// An operation the scenario needs failed, so nothing after it could be checked.
    pub const OPERATION_FAILED: &str = "effect_kit.operation_failed";
}

/// One finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The stable code, one of [`codes`].
    pub code: &'static str,
    /// The DocType of the scenario.
    pub doctype: String,
    /// The operation being checked: a lifecycle operation name, or `correct`.
    pub operation: &'static str,
    /// The step the finding is about, when one declares the store or class.
    pub step: Option<StepName>,
    /// The store the finding is about.
    pub store: Option<StoreName>,
    /// The change measured, for example `sum +10.00, rows +1`.
    pub delta: Option<String>,
    /// What is wrong.
    pub what: String,
    /// What to do about it.
    pub remedy: &'static str,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{} {}]", self.code, self.doctype, self.operation)?;
        if let Some(step) = &self.step {
            write!(f, " step {step}")?;
        }
        if let Some(store) = &self.store {
            write!(f, " store {store}")?;
        }
        if let Some(delta) = &self.delta {
            write!(f, " delta {delta}")?;
        }
        write!(f, ": {} Remedy: {}", self.what, self.remedy)
    }
}

/// The outcome of [`run`](crate::run).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// How many scenarios ran.
    pub scenarios: usize,
    /// Every finding, in the order found.
    pub violations: Vec<Violation>,
}

impl Report {
    /// Whether nothing was found.
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    /// The distinct codes found, sorted.
    pub fn codes(&self) -> Vec<&'static str> {
        let mut codes: Vec<&'static str> = self.violations.iter().map(|v| v.code).collect();
        codes.sort_unstable();
        codes.dedup();
        codes
    }

    /// Panics with every finding when there are any: the call an app's test makes.
    pub fn assert_clean(&self) {
        assert!(self.is_clean(), "effect conformance failed:\n{self}");
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for v in &self.violations {
            writeln!(f, "- {v}")?;
        }
        Ok(())
    }
}
