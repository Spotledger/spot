//! Errors: the stable `auditstore.*` family, plus the `audit.*` codes of the chain it wraps.

use deadpool_postgres::PoolError;
use spot_audit_chain::AuditError;
use tokio_postgres::Error as PgError;

/// The audit store could not complete the call: the database or its connection pool failed, or
/// the stored bytes could not be decoded.
pub const UNAVAILABLE: &str = "auditstore.unavailable";

/// The guard cannot be installed because a chain table is missing: the migrator's plan has not
/// run (or a table was dropped). Nothing was changed.
pub const TABLES_MISSING: &str = "auditstore.tables_missing";

/// The audit chain's protection is incomplete: a table, the guard trigger or the role's
/// restrictions are missing. A host refuses to serve on it.
pub const PROTECTION_INCOMPLETE: &str = "auditstore.protection_incomplete";

/// The appender state already holds a different `(lane, seq)` for an event id: the event was
/// chained twice, or the index was edited. Nothing was overwritten.
pub const EVENT_CONFLICT: &str = "auditstore.event_conflict";

/// A table of the durable appender state is missing or has the wrong columns: the migration plan
/// has not run. A host refuses to serve the site on it.
pub const APPENDER_TABLES_MISSING: &str = "auditstore.appender_tables_missing";

/// An error with a stable code. Messages never carry a payload value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The stable machine-readable code (`auditstore.unavailable`, `audit.record_invalid`, ...).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl Error {
    /// An error with `code` and `message`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Error {
        Error {
            code,
            message: message.into(),
        }
    }

    /// The store could not complete the call.
    pub fn unavailable(message: impl Into<String>) -> Error {
        Error::new(UNAVAILABLE, message)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

impl From<AuditError> for Error {
    fn from(error: AuditError) -> Error {
        Error::new(error.code(), error.to_string())
    }
}

/// Result of every fallible call in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A database failure, as a store error. No SQL statement is ever put in the message.
pub(crate) fn db(what: &str, error: &PgError) -> Error {
    match error.code() {
        Some(code) => Error::unavailable(format!(
            "{what}: the database refused the call (SQLSTATE {})",
            code.code()
        )),
        None => Error::unavailable(format!("{what}: the database connection failed")),
    }
}

/// A pool failure, as a store error.
pub(crate) fn pool(what: &str, error: &PoolError) -> Error {
    Error::unavailable(format!("{what}: {error}"))
}

/// A stored value that does not decode: the table was edited outside the append path.
pub(crate) fn corrupt(what: &str) -> Error {
    Error::unavailable(format!("a stored audit value is corrupt: {what}"))
}
