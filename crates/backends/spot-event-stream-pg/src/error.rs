//! Turning database and pool failures into the errors `ConsumerStore` carries.
//!
//! Every store method returns `StreamError`; a driver or pool failure is
//! `StreamError::Feed(StorageError)` with kind `Unavailable` (no connection) or `Internal`. The
//! text names the SQLSTATE class only: the driver's message can quote values and constraint
//! detail, and a store error is logged and enveloped.

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_event_stream::StreamError;
use spot_storage_contract::{ErrorKind, StorageError};

/// The consumer tables could not be created: an `event_stream_pg.*` code raised by `prepare`.
pub const MIGRATION_FAILED: &str = "event_stream_pg.migration_failed";

/// The SQLSTATE of a driver error, or `none` for a connection-level failure.
fn sqlstate(error: &tokio_postgres::Error) -> &str {
    error.code().map_or("none", |state| state.code())
}

/// A database failure while running a statement.
pub(crate) fn pg(error: &tokio_postgres::Error) -> StreamError {
    storage(
        ErrorKind::Internal,
        format!(
            "event stream store: database error (SQLSTATE {})",
            sqlstate(error)
        ),
    )
}

/// A pool failure (no connection could be checked out).
pub(crate) fn pool(_: PoolError) -> StreamError {
    storage(
        ErrorKind::Unavailable,
        "event stream store: no database connection".to_owned(),
    )
}

/// A stored row that cannot be read back (unparsable definition, impossible position).
pub(crate) fn corrupt(what: &str) -> StreamError {
    storage(
        ErrorKind::Internal,
        format!("event stream store: corrupt row ({what})"),
    )
}

fn storage(kind: ErrorKind, explanation: String) -> StreamError {
    StreamError::Feed(StorageError::new(kind, explanation))
}

/// A failed schema migration, as the `event_stream_pg.migration_failed` code.
pub(crate) fn migration(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the event stream consumer tables could not be created (SQLSTATE {})",
            sqlstate(&error)
        ),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(_: PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the event stream consumer tables could not be created: no connection".to_owned(),
    )
}
