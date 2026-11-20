//! Open conditions by severity.

use crate::count_value;

/// Gauge: open conditions by `severity`.
pub const OPEN: &str = "spot_conditions_open";

label_enum! {
    /// A condition's severity (mirrors `spot-conditions`; this crate does not depend on it).
    Severity {
        /// Nobody needs to know.
        Info => "info",
        /// Handled or minor.
        Amber => "amber",
        /// A person is needed.
        Red => "red",
    }
}

/// Open conditions per severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenCounts {
    /// Info.
    pub info: u64,
    /// Amber.
    pub amber: u64,
    /// Red.
    pub red: u64,
}

impl OpenCounts {
    /// The count of one severity.
    #[must_use]
    pub fn of(&self, severity: Severity) -> u64 {
        match severity {
            Severity::Info => self.info,
            Severity::Amber => self.amber,
            Severity::Red => self.red,
        }
    }
}

/// Sets the open conditions for **every** severity, so a cleared condition goes to 0 instead of
/// vanishing.
pub fn set_open(counts: &OpenCounts) {
    for severity in Severity::ALL {
        metrics::gauge!(OPEN, "severity" => severity.as_str())
            .set(count_value(counts.of(*severity)));
    }
}
