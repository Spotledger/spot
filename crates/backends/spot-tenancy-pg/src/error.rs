//! Turning database and pool failures into the errors the `spot-tenancy` traits carry, plus the
//! two codes this backend adds to the catalogue.
//!
//! The `ControlStore` trait returns [`spot_tenancy::Error`] and the ports return
//! [`StepFailure`](spot_tenancy::ports::StepFailure), whose `reason` is a fixed `&'static str`.
//! No failure therefore carries a raw SQL string, a connection string or a credential: a driver
//! or pool failure is reported as `tenancy.store_failed` (or a fixed port reason), and the
//! catalogue explains it. The two codes raised as [`AppError`]s are
//! [`MIGRATION_FAILED`] (the control or tenant schema could not be created) and
//! [`INVALID_BASELINE`] (the baseline catalogue cannot be merged with the built-in seed entity).

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_tenancy::Error;
use spot_tenancy::ports::StepFailure;

/// The control or tenant schema could not be created: a `tenancy_pg.*` code.
pub const MIGRATION_FAILED: &str = "tenancy_pg.migration_failed";

/// The baseline catalogue cannot be used: a `tenancy_pg.*` code.
pub const INVALID_BASELINE: &str = "tenancy_pg.invalid_baseline";

/// A failed statement while creating the control schema.
pub(crate) fn migration(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!("the tenancy control schema could not be created: {error}"),
    )
}

/// A pool failure while creating the control schema.
pub(crate) fn migration_pool(error: PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!("the tenancy control schema could not be created: no connection: {error}"),
    )
}

/// The baseline catalogue is not usable.
pub(crate) fn invalid_baseline() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        INVALID_BASELINE,
        "the baseline catalogue cannot be merged with the built-in seed entity",
    )
}

/// A control-store failure with a fixed reason.
pub(crate) fn store_failed(reason: &'static str) -> Error {
    Error::StoreFailed { reason }
}

/// A port failure with a fixed reason.
pub(crate) fn step(reason: &'static str, retryable: bool) -> StepFailure {
    StepFailure { retryable, reason }
}

/// Whether a driver error is a unique-constraint violation (`SQLSTATE 23505`), and which
/// constraint it names.
pub(crate) fn unique_violation(error: &tokio_postgres::Error) -> Option<&str> {
    let db = error.as_db_error()?;
    if db.code().code() == "23505" {
        db.constraint()
    } else {
        None
    }
}

/// Whether a driver error is "relation does not exist" (`SQLSTATE 42P01`).
pub(crate) fn is_undefined_table(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|db| db.code().code() == "42P01")
}
