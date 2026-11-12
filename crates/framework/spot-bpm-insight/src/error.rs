//! The crate's one error type, with stable `bpm_insight.*` codes.

/// Result of every fallible operation in this crate.
pub type InsightResult<T> = Result<T, InsightError>;

/// One problem while producing insight or its report definitions.
///
/// The codes are this crate's public vocabulary and their meaning never changes within a major
/// version; every code has one entry in `spot-errors`' `bpm_insight.toml`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InsightError {
    /// A report definition this crate ships is malformed or inconsistent.
    #[error("the report definition `{name}` is invalid: {detail}")]
    ReportDefinition {
        /// The report's name.
        name: String,
        /// A plain-language summary of the problems, one per report error.
        detail: String,
    },
    /// A run-label override is empty.
    #[error("the {which} run-label override is empty")]
    LabelInvalid {
        /// Which of the pair: `singular` or `plural`.
        which: String,
    },
    /// A stored accumulator state has a version this build does not know.
    #[error("the stored insight state has version {found}; this build reads version {supported}")]
    StateVersion {
        /// The version found in the state.
        found: u32,
        /// The version this build reads.
        supported: u32,
    },
    /// A stored accumulator state is inconsistent or corrupt.
    #[error("the stored insight state is corrupt: {detail}")]
    StateCorrupt {
        /// What is wrong, in plain language.
        detail: String,
    },
}

impl InsightError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ReportDefinition { .. } => "bpm_insight.report_definition",
            Self::LabelInvalid { .. } => "bpm_insight.label_invalid",
            Self::StateVersion { .. } => "bpm_insight.state_version",
            Self::StateCorrupt { .. } => "bpm_insight.state_corrupt",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_dotted() {
        let report = InsightError::ReportDefinition {
            name: "bpm_x".to_owned(),
            detail: "d".to_owned(),
        };
        assert_eq!(report.code(), "bpm_insight.report_definition");
        let label = InsightError::LabelInvalid {
            which: "singular".to_owned(),
        };
        assert_eq!(label.code(), "bpm_insight.label_invalid");
        assert!(spot_app_api::error::is_code(report.code()));
        assert!(spot_app_api::error::is_code(label.code()));
    }
}
