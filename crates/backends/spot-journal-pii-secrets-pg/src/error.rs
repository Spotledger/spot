//! Errors. A failure of the table is a [`PortError`] with a fixed sentence and a catalogue code;
//! it never carries a driver message (which can quote an entry id), a value, a wrapped key or a
//! connection string.

use std::fmt;

use spot_journal_pii_secrets::PortError;
use spot_pg_bridge::BridgeError;

/// The table is not in the database (or lacks an expected column): the migration has not run.
pub const TABLE_MISSING: &str = "journal_pii_secrets_pg.table_missing";

/// A read or write of the table failed (the database or the bridge): the answer is unknown.
pub const STORE_FAILED: &str = "journal_pii_secrets_pg.store_failed";

/// A stored row does not describe a live or destroyed key. The store fails closed on it.
pub const ROW_INVALID: &str = "journal_pii_secrets_pg.row_invalid";

/// Why [`crate::ensure_present`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableError {
    code: &'static str,
    message: String,
}

impl TableError {
    /// The stable dotted code: [`TABLE_MISSING`] or [`STORE_FAILED`].
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TableError {}

/// What a failure says without any row value: the bridge code, or the SQLSTATE of the database.
fn cause(error: &BridgeError<tokio_postgres::Error>) -> String {
    match error {
        BridgeError::Store(e) => e.code().map_or_else(
            || "the database call failed".to_owned(),
            |state| format!("SQLSTATE {}", state.code()),
        ),
        other => other
            .code()
            .map_or_else(|| "the call failed".to_owned(), str::to_owned),
    }
}

pub(crate) fn table_missing() -> TableError {
    TableError {
        code: TABLE_MISSING,
        message: format!(
            "{TABLE_MISSING}: the wrapped-key table is not in the database with its columns; run the schema migration with this crate's extra table"
        ),
    }
}

pub(crate) fn table_read_failed(error: BridgeError<tokio_postgres::Error>) -> TableError {
    TableError {
        code: STORE_FAILED,
        message: format!(
            "{STORE_FAILED}: the catalogue could not be read to check the wrapped-key table ({})",
            cause(&error)
        ),
    }
}

pub(crate) fn store_failed(error: BridgeError<tokio_postgres::Error>) -> PortError {
    PortError(format!(
        "{STORE_FAILED}: the wrapped-key table could not be read or written ({})",
        cause(&error)
    ))
}

pub(crate) fn row_invalid() -> PortError {
    PortError(format!(
        "{ROW_INVALID}: a stored row is neither a live key nor a tombstone"
    ))
}
