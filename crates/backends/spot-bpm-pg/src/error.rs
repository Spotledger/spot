//! Turning database, pool and row failures into the errors the engine's `CaseStore` trait
//! carries, plus the one code this backend adds to the catalogue.
//!
//! Every `CaseStore` method returns [`EngineError`], whose [`StoreFailed`](EngineError::StoreFailed)
//! variant carries a plain-language detail; a driver or pool failure, a corrupt stored row and a
//! duplicate case are all reported that way. A missing case is [`CaseNotFound`](EngineError::CaseNotFound)
//! and a stale version is [`VersionConflict`](EngineError::VersionConflict), exactly as the
//! in-memory reference reports them.
//!
//! The one code this crate raises on its own is [`MIGRATION_FAILED`], a `bpm_pg.*` code raised as
//! an [`AppError`] by [`PgCaseStore::prepare`](crate::PgCaseStore::prepare) when the schema DDL
//! cannot be applied. It is catalogued in `spot-errors` like every other code.

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_bpm_engine::{EngineError, EngineResult};

/// The BPM store's tables could not be created: a `bpm_pg.*` code raised by `prepare`.
pub const MIGRATION_FAILED: &str = "bpm_pg.migration_failed";

/// A database failure while running a statement, as an `EngineError`.
pub(crate) fn pg(error: tokio_postgres::Error) -> EngineError {
    store_failed(format!("bpm store: {error}"))
}

/// A pool failure (no connection could be checked out), as an `EngineError`.
pub(crate) fn pool(error: PoolError) -> EngineError {
    store_failed(format!("bpm store: no connection: {error}"))
}

/// A stored row that cannot be read back (unknown status text, negative version, bad id).
pub(crate) fn corrupt(what: impl Into<String>) -> EngineError {
    store_failed(format!("bpm store: {}", what.into()))
}

/// A `CaseStore` failure with a plain-language detail.
pub(crate) fn store_failed(detail: impl Into<String>) -> EngineError {
    EngineError::StoreFailed {
        detail: detail.into(),
    }
}

/// A failed schema migration, as the `bpm_pg.migration_failed` code.
pub(crate) fn migration(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!("the BPM store schema could not be created: {error}"),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(error: PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!("the BPM store schema could not be created: no connection: {error}"),
    )
}

/// Whether a driver error is a unique-constraint violation (`SQLSTATE 23505`).
pub(crate) fn is_unique_violation(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|db| db.code().code() == "23505")
}

/// The `EngineResult` alias the trait uses, re-exported for the module docs.
#[allow(dead_code)]
type Result<T> = EngineResult<T>;

/// A refused upgrade (both names exist, or a foreign table holds the old name), as the
/// `bpm_pg.migration_failed` code. The message carries table names only, never a row value.
pub(crate) fn migration_refused(message: String) -> AppError {
    AppError::new(ErrorClass::Internal, MIGRATION_FAILED, message)
}
