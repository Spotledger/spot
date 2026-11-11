//! Stable error codes for the app manifest, matching the `manifest.*` family of the native
//! API error registry (ADR-17).
//!
//! Parsing reports every problem at once as a list of these errors. Callers match on
//! [`ManifestError::code`], never on the message: the codes are this crate's API contract.

use std::fmt;

/// Stable machine-readable codes for `manifest.*`, matching the native API error registry.
/// Callers match on the code, never on the message.
pub(crate) const INVALID_TOML: &str = "manifest.invalid_toml";
pub(crate) const UNKNOWN_KEY: &str = "manifest.unknown_key";
pub(crate) const MISSING_KEY: &str = "manifest.missing_key";
pub(crate) const INVALID_TYPE: &str = "manifest.invalid_type";
pub(crate) const INVALID_NAME: &str = "manifest.invalid_name";
pub(crate) const INVALID_VERSION: &str = "manifest.invalid_version";
pub(crate) const INVALID_EVENT: &str = "manifest.invalid_event";
pub(crate) const PHASE_RULE: &str = "manifest.phase_rule";
pub(crate) const INVALID_SESSION_EVENT: &str = "manifest.invalid_session_event";
pub(crate) const INVALID_STEP: &str = "manifest.invalid_step";
pub(crate) const FOREIGN_STEP: &str = "manifest.foreign_step";
pub(crate) const DUPLICATE_STEP: &str = "manifest.duplicate_step";
pub(crate) const BUDGET_OUT_OF_RANGE: &str = "manifest.budget_out_of_range";
pub(crate) const DUPLICATE_SCHEDULE: &str = "manifest.duplicate_schedule";
pub(crate) const INVALID_CRON: &str = "manifest.invalid_cron";
pub(crate) const INVALID_QUEUE: &str = "manifest.invalid_queue";
pub(crate) const INVALID_INTENT: &str = "manifest.invalid_intent";
pub(crate) const DUPLICATE_PATCH: &str = "manifest.duplicate_patch";
pub(crate) const PATCH_ORDER: &str = "manifest.patch_order";
pub(crate) const OVERRIDE_MISSING_BY: &str = "manifest.override_missing_by";
pub(crate) const OVERRIDE_WITH_BY: &str = "manifest.override_with_by";
pub(crate) const INVALID_OVERRIDE: &str = "manifest.invalid_override";
pub(crate) const INVALID_SEAM: &str = "manifest.invalid_seam";
pub(crate) const INVALID_PATH: &str = "manifest.invalid_path";
pub(crate) const INVALID_ROUTE: &str = "manifest.invalid_route";
pub(crate) const INVALID_METHOD: &str = "manifest.invalid_method";
pub(crate) const INVALID_CAPABILITY: &str = "manifest.invalid_capability";
pub(crate) const PERMISSION_NAME: &str = "manifest.permission_name";
pub(crate) const PERMISSION_TITLE: &str = "manifest.permission_title";
pub(crate) const PERMISSION_DUPLICATE: &str = "manifest.permission_duplicate";

/// One problem with a manifest.
///
/// `code` is stable and callers match on it, not on the message. `line` and `column` (both
/// 1-based; the column counts characters) point at the offending TOML item when the manifest
/// was parsed from a string; manifests built in code leave them unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    /// The stable machine-readable code, for the API error envelope.
    pub code: &'static str,
    /// What went wrong, phrased for the app author.
    pub message: String,
    /// The 1-based line, when the manifest came from a file.
    pub line: Option<u32>,
    /// The 1-based column in characters, when the manifest came from a file.
    pub col: Option<u32>,
}

impl ManifestError {
    /// An error without a position.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            line: None,
            col: None,
        }
    }

    /// Attaches the position, for errors found while parsing a file.
    #[must_use]
    pub fn at(mut self, line: u32, col: u32) -> Self {
        self.line = Some(line);
        self.col = Some(col);
        self
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let (Some(line), Some(col)) = (self.line, self.col) {
            write!(f, " (line {line}, column {col})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ManifestError {}

/// Maps a byte offset in `source` to a 1-based line and column. The column counts characters,
/// so for ASCII input it matches any byte-based count too.
pub fn line_col(source: &str, offset: usize) -> (u32, u32) {
    let mut end = offset.min(source.len());
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let before = &source[..end];
    let line = before.matches('\n').count() as u32 + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = before[line_start..].chars().count() as u32 + 1;
    (line, col)
}
