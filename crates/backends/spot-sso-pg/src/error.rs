//! Turning database and pool failures into [`ReplayError`], and the one code this backend adds to
//! the catalogue.
//!
//! A claim or a prune that cannot be answered is [`ReplayError::unavailable`] and nothing else:
//! the store keeps no driver text, SQLSTATE or key value, because the value it would quote is the
//! single-use identifier of a signed message. The schema migration is the one operation that has
//! an operator to tell, so it reports its SQLSTATE (never a row value) under
//! [`MIGRATION_FAILED`].

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_sso::ReplayError;

/// The replay store's table could not be created: raised by `prepare`.
pub const MIGRATION_FAILED: &str = "sso_pg.migration_failed";

/// What a database failure says, without any value: the SQLSTATE, or a fixed sentence.
fn describe(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("database error {}", db.code().code()),
        None if error.is_closed() => "the connection was closed".to_owned(),
        None => "the database could not be reached or answered unexpectedly".to_owned(),
    }
}

/// A database failure while claiming or pruning: the store cannot vouch for the id, so it refuses.
pub(crate) fn unavailable(_error: &tokio_postgres::Error) -> ReplayError {
    ReplayError::unavailable()
}

/// A pool failure: no connection could be checked out, so the store refuses.
pub(crate) fn pool_unavailable(_error: &PoolError) -> ReplayError {
    ReplayError::unavailable()
}

/// A failed schema migration, as the `sso_pg.migration_failed` code.
pub(crate) fn migration(error: &tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the replay store table could not be created: {}",
            describe(error)
        ),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(_error: &PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the replay store table could not be created: no database connection is available",
    )
}

/// A schema migration this crate refuses to guess at (both table names present, or an old table
/// that is not ours). The message names tables only, never a row.
pub(crate) fn migration_refused(message: String) -> AppError {
    AppError::new(ErrorClass::Internal, MIGRATION_FAILED, message)
}
