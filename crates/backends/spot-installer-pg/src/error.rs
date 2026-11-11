//! Turning database and pool failures into the [`ExecError`] the registry trait carries.
//!
//! Messages name the failing operation and, for a driver error, only its SQLSTATE: the driver's
//! own text can quote a statement's parameters, and a registry row holds capabilities and undo
//! data, so it never reaches an error, a log line or a `Debug` output.

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_installer::ExecError;

/// The registry tables could not be created: raised as an [`AppError`] by `prepare`.
pub const MIGRATION_FAILED: &str = "installer_pg.migration_failed";
/// The installer's own code for a refused step, so the executor reports a refusal like any
/// other failed step (the same text as `spot-installer`'s `installer.step_failed`).
pub const STEP_FAILED: &str = "installer.step_failed";
/// The app is already installed with a different version or digest (`installer.already_installed`).
pub const ALREADY_INSTALLED: &str = "installer.already_installed";
/// The app is not installed (`installer.not_installed`).
pub const NOT_INSTALLED: &str = "installer.not_installed";
/// Another installer already holds the site (`installer.lease_held`).
pub const LEASE_HELD: &str = "installer.lease_held";
/// A statement or a connection failed.
pub const STORAGE_FAILED: &str = "installer_pg.storage_failed";
/// A stored row cannot be read back (unknown text, out-of-range number, bad JSON shape).
pub const CORRUPT_ROW: &str = "installer_pg.corrupt_row";

/// The SQLSTATE of a driver error, or a fixed word when it carries none.
fn state(error: &tokio_postgres::Error) -> String {
    error
        .code()
        .map_or_else(|| "no sqlstate".to_owned(), |code| code.code().to_owned())
}

/// A database failure while running a statement.
pub(crate) fn pg(operation: &str, error: &tokio_postgres::Error) -> ExecError {
    ExecError::new(
        STORAGE_FAILED,
        format!(
            "installer registry: {operation} failed (sqlstate {})",
            state(error)
        ),
    )
}

/// A pool failure: no connection could be checked out.
pub(crate) fn pool(operation: &str, _error: &PoolError) -> ExecError {
    ExecError::new(
        STORAGE_FAILED,
        format!("installer registry: {operation} found no database connection"),
    )
}

/// A stored row that cannot be read back, or a value that cannot be stored.
pub(crate) fn corrupt(what: &str) -> ExecError {
    ExecError::new(CORRUPT_ROW, format!("installer registry: {what}"))
}

/// A failed schema migration.
pub(crate) fn migration(error: &tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the installer registry schema could not be created (sqlstate {})",
            state(error)
        ),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(_error: &PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the installer registry schema could not be created: no database connection",
    )
}

/// A schema migration this crate refuses to guess at (both names of a table present, an old table
/// that is not ours, or a name already taken). The message names tables only, never a row.
pub(crate) fn migration_refused(message: String) -> AppError {
    AppError::new(ErrorClass::Internal, MIGRATION_FAILED, message)
}
