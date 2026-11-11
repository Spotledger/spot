//! The failure a Document node reports.

/// Why one document operation did not happen.
///
/// The handler turns this into [`NodeOutcome::Fail`](spot_bpm_engine::NodeOutcome::Fail), so the
/// engine retries per the node's policy and then takes the node's error branch. The `code` is a
/// stable `bpm_documents.*` code (or the document engine's own code, for example
/// `document.required` or `permission.denied`); the `reason` is a plain-language sentence that
/// names no field value and no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentFailure {
    code: String,
    reason: String,
}

impl DocumentFailure {
    /// A failure with a stable code and a plain-language reason.
    #[must_use]
    pub fn new(code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            reason: reason.into(),
        }
    }

    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The plain-language reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl core::fmt::Display for DocumentFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.code, self.reason)
    }
}

impl std::error::Error for DocumentFailure {}
