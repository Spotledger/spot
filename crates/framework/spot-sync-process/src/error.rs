//! Stable `sync_process.*` codes.

use std::fmt;

/// The definition hash does not match the record's, or a version changed under the same name.
pub const HASH_MISMATCH: &str = "sync_process.hash_mismatch";
/// A process record came from an edge: process definitions flow down only.
pub const EDGE_AUTHORED: &str = "sync_process.edge_authored";
/// The record is for another tenant, or its `process` does not match its payload or name.
pub const FOREIGN_RECORD: &str = "sync_process.foreign_record";
/// The record does not parse, or its name is not `process:<process>@<version>`.
pub const INVALID_RECORD: &str = "sync_process.invalid_record";
/// The store failed.
pub const STORE_FAILED: &str = "sync_process.store_failed";

/// Every code of this crate.
pub const CODES: [&str; 5] = [
    HASH_MISMATCH,
    EDGE_AUTHORED,
    FOREIGN_RECORD,
    INVALID_RECORD,
    STORE_FAILED,
];

/// A refusal. `message` names processes and versions only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The stable code.
    pub code: &'static str,
    /// What happened, without definition content.
    pub message: String,
}

/// The crate's result type.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// An error with `code`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}
