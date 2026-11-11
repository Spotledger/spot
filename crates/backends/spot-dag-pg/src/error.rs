//! Turning database and pool failures into the errors the `ExecStore` trait carries, plus the
//! one code this backend adds to the catalogue.
//!
//! Every `ExecStore` method returns `spot_dag_exec::ExecError`, so a driver or pool failure is
//! reported as [`ExecError::StorageFailed`](spot_dag_exec::ExecError::StorageFailed) with a
//! storage [`ErrorKind`]: a pool that timed out or a connection that could not be opened is
//! `Unavailable`, anything else is `Internal`. A corrupt stored row (an unknown status text, a
//! negative version) is `Internal` too.
//!
//! The one code this crate raises on its own is [`MIGRATION_FAILED`], a `dag_pg.*` code raised
//! as an [`AppError`] by [`PgExecStore::prepare`](crate::PgExecStore::prepare) when the schema
//! DDL cannot be applied. It is catalogued in `spot-errors` like every other code.

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_dag_exec::ExecError;
use spot_storage_contract::{ErrorKind, StorageError};

/// The DAG store's tables could not be created: a `dag_pg.*` code raised by `prepare`.
pub const MIGRATION_FAILED: &str = "dag_pg.migration_failed";

/// A database failure while running a statement, as an `ExecStore` error.
pub(crate) fn pg(error: tokio_postgres::Error) -> ExecError {
    storage(ErrorKind::Internal, format!("dag store: {error}"))
}

/// A pool failure (no connection could be checked out), as an `ExecStore` error.
pub(crate) fn pool(error: PoolError) -> ExecError {
    storage(
        ErrorKind::Unavailable,
        format!("dag store: no connection: {error}"),
    )
}

/// A stored row that cannot be read back (unknown status text, negative version, bad id).
pub(crate) fn corrupt(what: impl Into<String>) -> ExecError {
    storage(ErrorKind::Internal, format!("dag store: {}", what.into()))
}

fn storage(kind: ErrorKind, explanation: String) -> ExecError {
    ExecError::StorageFailed(StorageError::new(kind, explanation))
}

/// A failed schema migration, as the `dag_pg.migration_failed` code.
pub(crate) fn migration(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the dag store schema could not be created: {}",
            describe(&error)
        ),
    )
}

/// What a database error says without any row value: the message and the SQLSTATE, never the
/// detail (which can quote the key of a row).
fn describe(error: &tokio_postgres::Error) -> String {
    error.as_db_error().map_or_else(
        || "the database call failed".to_owned(),
        |db| format!("{} (SQLSTATE {})", db.message(), db.code().code()),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(error: PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!("the dag store schema could not be created: no connection: {error}"),
    )
}

/// A schema migration this crate refuses to guess at (both names of a table present, an old table
/// that is not ours, or a name already taken). The message names tables only, never a row.
pub(crate) fn migration_refused(message: String) -> AppError {
    AppError::new(ErrorClass::Internal, MIGRATION_FAILED, message)
}
