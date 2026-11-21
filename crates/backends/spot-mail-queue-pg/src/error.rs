//! Turning database and pool failures into the store's errors, and the one code this crate adds
//! to the catalogue.
//!
//! The port's failure is [`Unavailable`], a value with no fields on purpose: two of the three
//! methods report it, a caller can do nothing with the detail, and the details of a failing
//! connection include the host, the database name and sometimes a bound value. So a runtime
//! failure is built here and nothing of the driver's message, SQLSTATE or the statement is kept.
//! A queue that cannot read the log retries; it does not need to know why.
//!
//! `prepare` is not part of the port and does explain itself, so it reports the catalogue code
//! `mail_queue_pg.migration_failed` as an [`AppError`] — with the SQLSTATE of a database failure
//! (a five-character class and code, never a message) or one fixed sentence. That is the only
//! text this crate produces.

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_mail_queue::Unavailable;

/// The sent-log table could not be created: raised by
/// [`prepare`](crate::PgSentLog::prepare).
pub const MIGRATION_FAILED: &str = "mail_queue_pg.migration_failed";

/// What a database failure says, without any value: its SQLSTATE, or a fixed sentence.
fn describe(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("database error {}", db.code().code()),
        None if error.is_closed() => "the connection was closed".to_owned(),
        None => "the database could not be reached or answered unexpectedly".to_owned(),
    }
}

/// A database failure while running a statement: the port's `Unavailable`, with nothing carried.
pub(crate) fn unavailable(_error: &tokio_postgres::Error) -> Unavailable {
    Unavailable
}

/// A pool failure — no connection could be checked out: also `Unavailable`.
pub(crate) fn unavailable_pool(_error: &PoolError) -> Unavailable {
    Unavailable
}

/// A stored row this version of the store cannot read (an address that no longer parses) is
/// reported as `Unavailable` rather than guessed at: the queue then treats the log as unreadable
/// and retries, which is the safe direction for a message that may have been sent.
pub(crate) fn unreadable() -> Unavailable {
    Unavailable
}

/// A failed schema migration, as the `mail_queue_pg.migration_failed` code.
pub(crate) fn migration(error: &tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the sent log's table could not be created: {}",
            describe(error)
        ),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(_error: &PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the sent log's table could not be created: no database connection is available",
    )
}

/// The old and the new table both exist: `prepare` refuses to pick one and drops neither.
pub(crate) fn migration_both_tables(old: &str, new: &str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the sent log's table could not be upgraded: both `{old}` and `{new}` exist; neither \
             was changed, and one of them must be merged into the other by an operator"
        ),
    )
}
