//! The crate's error type: a stable `custom_bundle.*` code with a plain-language message.
//!
//! Every code here has one entry in `crates/framework/spot-errors/catalogue/custom_bundle.toml`
//! with a summary, the parameters its message names and a remedy.

use std::fmt;

/// The stable error kind, one variant per `custom_bundle.*` code.
///
/// The code is a string, not a Rust enum discriminant, so it survives serialisation and the
/// catalogue stays the source of truth for what a code means.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CustomBundleError {
    /// The stable code, for example `custom_bundle.digest_mismatch`.
    code: String,
    /// The item the error is about (a file, a DocType, an app), when there is one.
    item: String,
    /// One plain-language sentence; never a secret and never a raw query.
    message: String,
}

/// The `custom_bundle.*` codes this crate raises. Kept as constants so the raise sites and the
/// catalogue can be compared mechanically.
pub mod codes {
    /// The bytes are not a ZIP archive at all.
    pub const BAD_ARCHIVE: &str = "custom_bundle.bad_archive";
    /// An entry does not decompress, or its CRC does not match.
    pub const BAD_DEFLATE: &str = "custom_bundle.bad_deflate";
    /// A symlink entry; bundles carry plain files and directories only.
    pub const BAD_SYMLINK: &str = "custom_bundle.bad_symlink";
    /// Two entries normalise to the same name, including two that differ only by case.
    pub const DUPLICATE_ENTRY: &str = "custom_bundle.duplicate_entry";
    /// A name that escapes the bundle or is not a plain relative path.
    pub const ILLEGAL_PATH: &str = "custom_bundle.illegal_path";
    /// A required entry (`bundle.toml` or `digest`) is absent.
    pub const MISSING_ENTRY: &str = "custom_bundle.missing_entry";
    /// The archive, one file, or the total inflated size exceeds the limits.
    pub const TOO_LARGE: &str = "custom_bundle.too_large";
    /// More entries than the limits allow.
    pub const TOO_MANY_ENTRIES: &str = "custom_bundle.too_many_entries";
    /// An entry that is not `bundle.toml`, `digest`, or under `sections/`.
    pub const UNEXPECTED_ENTRY: &str = "custom_bundle.unexpected_entry";
    /// The bundle's format version is greater than what this reader supports.
    pub const FORMAT_TOO_NEW: &str = "custom_bundle.format_too_new";
    /// An app the bundle customises is at a different major version on the target.
    pub const APP_INCOMPATIBLE: &str = "custom_bundle.app_incompatible";
    /// An app the bundle customises is at a different minor or patch version (a warning).
    pub const APP_VERSION: &str = "custom_bundle.app_version";
    /// An app the bundle customises is not installed on the target.
    pub const APP_MISSING: &str = "custom_bundle.app_missing";
    /// The stored digest does not match the bundle's content.
    pub const DIGEST_MISMATCH: &str = "custom_bundle.digest_mismatch";
    /// The bundle contains a section kind this reader does not know and `allow_unknown` is off.
    pub const UNKNOWN_SECTION: &str = "custom_bundle.unknown_section";
    /// The meta section fails validation when rebuilt against the target's definitions.
    pub const INVALID_META: &str = "custom_bundle.invalid_meta";
    /// A view layer fails resolution or validation against the target's metadata.
    pub const INVALID_VIEWS: &str = "custom_bundle.invalid_views";
    /// A rule expression does not parse.
    pub const INVALID_RULES: &str = "custom_bundle.invalid_rules";
    /// A script's recorded hash does not match its source.
    pub const INVALID_SCRIPTS: &str = "custom_bundle.invalid_scripts";
    /// A fixtures file is not a JSON array of documents.
    pub const INVALID_FIXTURES: &str = "custom_bundle.invalid_fixtures";
    /// A change could not be written; the transaction was rolled back.
    pub const APPLY_FAILED: &str = "custom_bundle.apply_failed";
    /// Capture or apply touched an item the target marks system-managed or secret.
    pub const SYSTEM_MANAGED: &str = "custom_bundle.system_managed";
}

impl CustomBundleError {
    /// An error with a code and a message, naming no item.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            item: String::new(),
            message: message.into(),
        }
    }

    /// An error that names the offending item (a file, a DocType, an app).
    pub fn about(
        code: impl Into<String>,
        item: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            item: item.into(),
            message: message.into(),
        }
    }

    /// An error built from a section check's [`Problem`](crate::Problem).
    pub fn from_problem(problem: crate::registry::Problem) -> Self {
        Self::about(problem.code, problem.item, problem.message)
    }

    /// The stable code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The item the error is about, if any.
    pub fn item(&self) -> &str {
        &self.item
    }

    /// The plain-language message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CustomBundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.item.is_empty() {
            write!(f, "{}: {}", self.code, self.message)
        } else {
            write!(f, "{}: {}: {}", self.code, self.item, self.message)
        }
    }
}

impl std::error::Error for CustomBundleError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_code_and_item() {
        let with_item = CustomBundleError::about(codes::APP_MISSING, "crm", "not installed");
        assert_eq!(
            with_item.to_string(),
            "custom_bundle.app_missing: crm: not installed"
        );
        let without_item = CustomBundleError::new(codes::BAD_ARCHIVE, "not a zip");
        assert_eq!(
            without_item.to_string(),
            "custom_bundle.bad_archive: not a zip"
        );
    }
}
