//! The crate's one error type and its stable `bulk.*` codes (`bulk.toml`).
//!
//! A message names paths, counts and codes only: never a field value, and never a raw SQL
//! string. A refusal the field guard made rides through unchanged so the caller sees the
//! guard's own code (`permission.field_denied` naming every refused path).

use spot_field_guard::GuardError;
use spot_storage_contract::{ErrorKind, StorageError};

/// The export cursor or extraction watermark is malformed, forged, expired, or belongs to
/// another caller or request.
pub const CURSOR_INVALID: &str = "bulk.cursor_invalid";
/// The watermark is older than the change feed retains or from before a resync, so a partial
/// result would be wrong.
pub const WATERMARK_EXPIRED: &str = "bulk.watermark_expired";
/// Too many exports or extractions are in flight, or the rows-per-second budget is used up.
pub const LIMIT: &str = "bulk.limit";
/// The principal does not hold the `export` right on the DocType.
pub const EXPORT_DENIED: &str = "bulk.export_denied";
/// The export specification is not usable (no columns, an order key that is not a plain
/// field, a duplicate column).
pub const SPEC_INVALID: &str = "bulk.spec_invalid";
/// The storage backend failed while a page was being read.
pub const STORAGE: &str = "bulk.storage";

/// A bulk export or extraction failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkError {
    code: &'static str,
    message: String,
}

impl BulkError {
    /// An error of `code` with a plain-language message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> BulkError {
        BulkError {
            code,
            message: message.into(),
        }
    }

    /// A malformed, forged or foreign cursor.
    #[must_use]
    pub fn cursor_invalid(message: impl Into<String>) -> BulkError {
        BulkError::new(CURSOR_INVALID, message)
    }

    /// A watermark that can no longer be continued.
    #[must_use]
    pub fn watermark_expired(message: impl Into<String>) -> BulkError {
        BulkError::new(WATERMARK_EXPIRED, message)
    }

    /// A bound or a rate budget is used up.
    #[must_use]
    pub fn limit(message: impl Into<String>) -> BulkError {
        BulkError::new(LIMIT, message)
    }

    /// The principal may not export this DocType.
    #[must_use]
    pub fn export_denied(doctype: &str) -> BulkError {
        BulkError::new(
            EXPORT_DENIED,
            format!("the caller does not hold the export right on `{doctype}`"),
        )
    }

    /// The specification is not usable.
    #[must_use]
    pub fn spec_invalid(message: impl Into<String>) -> BulkError {
        BulkError::new(SPEC_INVALID, message)
    }

    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// What the caller can tell the user.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for BulkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BulkError {}

impl From<GuardError> for BulkError {
    fn from(e: GuardError) -> Self {
        BulkError::new(e.code(), e.to_string())
    }
}

impl From<StorageError> for BulkError {
    fn from(e: StorageError) -> Self {
        if e.kind == ErrorKind::ResyncRequired {
            BulkError::watermark_expired(e.explanation)
        } else {
            BulkError::new(STORAGE, format!("{}: {}", e.kind.code(), e.explanation))
        }
    }
}

/// The crate's result alias.
pub type BulkResult<T> = Result<T, BulkError>;
