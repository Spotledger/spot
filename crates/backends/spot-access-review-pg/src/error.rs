//! Turning database and pool failures into the stores' fixed-sentence errors.
//!
//! A failure the store cannot answer is `Unavailable` and nothing else: no driver text, no
//! SQLSTATE and no value is carried, because a value here can be a user, a note or a reason.
//! The schema migration is the one operation with an operator to tell; it reports a SQLSTATE
//! (never a row value) under [`MIGRATION_FAILED`].

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use tokio_postgres::error::SqlState;

/// The stores' tables could not be created: raised by `prepare`.
pub const MIGRATION_FAILED: &str = "access_review_pg.migration_failed";

/// Whether the failure is a unique-constraint violation (a lost race for a key).
pub(crate) fn is_unique(error: &tokio_postgres::Error) -> bool {
    error.code() == Some(&SqlState::UNIQUE_VIOLATION)
}

pub(crate) fn campaign_unavailable(
    _error: &tokio_postgres::Error,
) -> spot_access_review::StoreError {
    spot_access_review::StoreError::Unavailable
}

pub(crate) fn campaign_pool(_error: &PoolError) -> spot_access_review::StoreError {
    spot_access_review::StoreError::Unavailable
}

pub(crate) fn sod_unavailable(_error: &tokio_postgres::Error) -> spot_sod::SodStoreError {
    spot_sod::SodStoreError::Unavailable
}

pub(crate) fn sod_pool(_error: &PoolError) -> spot_sod::SodStoreError {
    spot_sod::SodStoreError::Unavailable
}

/// A failed schema migration: the SQLSTATE or a fixed sentence, never a row.
pub(crate) fn migration(error: &tokio_postgres::Error) -> AppError {
    let why = match error.as_db_error() {
        Some(db) => format!("database error {}", db.code().code()),
        None if error.is_closed() => "the connection was closed".to_owned(),
        None => "the database could not be reached or answered unexpectedly".to_owned(),
    };
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!("the access review tables could not be created: {why}"),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(_error: &PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the access review tables could not be created: no database connection is available",
    )
}
