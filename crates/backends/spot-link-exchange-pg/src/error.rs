//! Errors: the `link_exchange_pg.*` code this backend adds, and the mapping of database failures
//! onto the fixed-reason errors the `spot-link-exchange` traits carry.
//!
//! No failure carries a SQL string, a driver message, a connection string or a value: a driver
//! or pool failure becomes `link_exchange.store_failed` with a fixed sentence, and a lost race
//! becomes `link_exchange.conflict`, which the inbox and outbox already retry.

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_link_exchange::Error;

/// The tenant schema's exchange tables could not be created.
pub const MIGRATION_FAILED: &str = "link_exchange_pg.migration_failed";

/// A failed statement while creating the tables.
pub(crate) fn migration(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the link exchange tables could not be created: {}",
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

/// A schema migration this crate refuses to guess at (both names of a table present, an old table
/// that is not ours, or a name already taken). The message names tables only, never a row.
pub(crate) fn migration_refused(message: String) -> AppError {
    AppError::new(ErrorClass::Internal, MIGRATION_FAILED, message)
}

/// A pool failure while creating the tables.
pub(crate) fn migration_pool(_error: PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the link exchange tables could not be created: no connection is available",
    )
}

/// A store failure with a fixed reason.
pub(crate) fn store_failed(reason: &'static str) -> Error {
    Error::StoreFailed { reason }
}

/// A lost race.
pub(crate) fn conflict(reason: &'static str) -> Error {
    Error::Conflict { reason }
}

/// The failure for a driver error on a read or write. A unique violation, a serialization
/// failure and a deadlock are lost races (`Conflict`, retried by the caller); anything else is a
/// store failure.
pub(crate) fn query_failed(error: &tokio_postgres::Error) -> Error {
    match error.as_db_error().map(|db| db.code().code()) {
        Some("23505") => conflict("a row in the plan already exists"),
        Some("40001" | "40P01") => conflict("the transaction lost a race and may be retried"),
        _ => store_failed("the exchange store query failed"),
    }
}

/// `query_failed` as a function a `map_err` can take.
pub(crate) fn q(error: tokio_postgres::Error) -> Error {
    query_failed(&error)
}
