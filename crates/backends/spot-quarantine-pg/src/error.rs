//! Turning database and pool failures into [`QuarantineError`], and the one code this backend
//! adds to the catalogue.
//!
//! No error here carries a value from a record or the database's own message text (which can
//! quote a key): a database failure is reported as its SQLSTATE, a pool failure as a fixed
//! sentence.

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_quarantine::QuarantineError;

/// The quarantine store's tables could not be created: raised by `prepare`.
pub const MIGRATION_FAILED: &str = "quarantine_pg.migration_failed";

/// What a database failure says, without any value: the SQLSTATE, or a fixed sentence.
fn describe(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("database error {}", db.code().code()),
        None if error.is_closed() => "the connection was closed".to_owned(),
        None => "the database could not be reached or answered unexpectedly".to_owned(),
    }
}

/// A database failure while running a statement.
pub(crate) fn pg(error: &tokio_postgres::Error) -> QuarantineError {
    QuarantineError::Store {
        why: describe(error),
    }
}

/// A pool failure: no connection could be checked out.
pub(crate) fn pool(_error: &PoolError) -> QuarantineError {
    QuarantineError::Store {
        why: "no database connection is available".to_owned(),
    }
}

/// A stored row or an argument that cannot be represented, with a fixed explanation.
pub(crate) fn invalid(why: &str) -> QuarantineError {
    QuarantineError::Store {
        why: why.to_owned(),
    }
}

/// A failed schema migration, as the `quarantine_pg.migration_failed` code.
pub(crate) fn migration(error: &tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the quarantine store schema could not be created: {}",
            describe(error)
        ),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(_error: &PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the quarantine store schema could not be created: no database connection is available",
    )
}

/// A schema migration this crate refuses to guess at (both names of a table present, an old table
/// that is not ours, or a name already taken). The message names tables only, never a row.
pub(crate) fn migration_refused(message: String) -> AppError {
    AppError::new(ErrorClass::Internal, MIGRATION_FAILED, message)
}

/// A failure of the read-only state-grant check. It reuses the migration code: the check is part
/// of preparing a database for use, and the message says which step failed.
pub(crate) fn grant_check(error: &tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the quarantine state-grant check could not run: {}",
            describe(error)
        ),
    )
}
