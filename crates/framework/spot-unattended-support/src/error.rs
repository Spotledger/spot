//! The crate's error type and its stable codes (all catalogued in `unattended_support.toml`).

use std::fmt;

/// Stable codes this crate raises.
pub mod codes {
    /// A help file does not parse.
    pub const HELP_MALFORMED: &str = "unattended_support.help_malformed";
    /// A user-visible code has no help.
    pub const HELP_MISSING: &str = "unattended_support.help_missing";
    /// A help entry names a code the catalogue lacks.
    pub const HELP_ORPHAN: &str = "unattended_support.help_orphan";
    /// A help entry points to a document that does not exist.
    pub const HELP_BAD_DOC: &str = "unattended_support.help_bad_doc";
    /// A help entry breaks the style rules.
    pub const HELP_STYLE: &str = "unattended_support.help_style";
    /// A consent value is not off, ask or on.
    pub const CONSENT_INVALID: &str = "unattended_support.consent_invalid";
    /// The spool could not be read or written.
    pub const SPOOL_FAILED: &str = "unattended_support.spool_failed";
    /// The redaction rules could not be built.
    pub const REDACTOR_FAILED: &str = "unattended_support.redactor_failed";
    /// A report breaks the size or shape rules.
    pub const REPORT_INVALID: &str = "unattended_support.report_invalid";
    /// A report is over the size cap even after trimming.
    pub const REPORT_TOO_LARGE: &str = "unattended_support.report_too_large";
    /// A telemetry counter name is not allowed.
    pub const NAME_INVALID: &str = "unattended_support.name_invalid";
    /// An upload did not complete.
    pub const UPLOAD_FAILED: &str = "unattended_support.upload_failed";
    /// A work item sink failed.
    pub const SINK_FAILED: &str = "unattended_support.sink_failed";
}

/// An error with a stable code and a message that never carries a secret or report content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportError {
    code: &'static str,
    message: String,
}

impl SupportError {
    /// A new error. `message` must name no secret and no document content.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SupportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SupportError {}
