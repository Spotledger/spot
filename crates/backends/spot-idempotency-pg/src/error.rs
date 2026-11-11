//! Turning database and pool failures into [`IdempotencyError`], and the one code this backend
//! adds to the catalogue.
//!
//! A reservation, completion, wait or prune that cannot be answered is
//! [`IdempotencyError::unavailable`] and nothing else: the store keeps no driver text, SQLSTATE,
//! host, key, fingerprint or body, because an `Idempotency-Key` and the response it guards belong
//! to a caller. The schema migration is the one operation that has an operator to tell, so it
//! reports its SQLSTATE (never a row value) under [`MIGRATION_FAILED`].

use spot_app_api::{AppError, ErrorClass};
use spot_http::api::IdempotencyError;

/// The idempotency table could not be created: raised by `prepare`.
pub const MIGRATION_FAILED: &str = "idempotency_pg.migration_failed";

/// What a database failure says, without any value: the SQLSTATE, or a fixed sentence.
fn describe(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("database error {}", db.code().code()),
        None if error.is_closed() => "the connection was closed".to_owned(),
        None => "the database could not be reached or answered unexpectedly".to_owned(),
    }
}

/// Any failure while serving a request: the store cannot vouch for the key, so it refuses.
pub(crate) fn unavailable() -> IdempotencyError {
    IdempotencyError::unavailable()
}

/// A failed schema migration, as the `idempotency_pg.migration_failed` code.
pub(crate) fn migration(error: &tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the idempotency table could not be created: {}",
            describe(error)
        ),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the idempotency table could not be created: no database connection is available",
    )
}
