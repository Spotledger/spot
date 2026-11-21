//! Turning database and pool failures into the errors the `LedgerStore` trait carries, plus the
//! one code this backend adds to the catalogue.
//!
//! Every [`LedgerStore`](spot_effect_hold::LedgerStore) method returns
//! [`spot_effect_hold::Error`], and every way the database can fail — a pool with no connection, a
//! statement the server refused, a stored row that does not decode — becomes
//! [`Error::LedgerUnavailable`]. That is the error the hold treats as "the state is unknown, stay
//! held", so a broken ledger can never open a site or report an empty effect list.
//!
//! A reason names the operation and what the driver or the server said. It never carries SQL text
//! or a bound value: the messages are logged and shipped.
//!
//! The one code this crate raises on its own is [`MIGRATION_FAILED`], an `effect_hold_pg.*` code
//! raised as an [`AppError`] by [`PgLedger::prepare`](crate::PgLedger::prepare) when the schema
//! DDL cannot be applied. It is catalogued in `spot-errors` like every other code.

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_effect_hold::Error;
use tokio_postgres::Error as PgError;

/// The ledger's tables could not be created: an `effect_hold_pg.*` code raised by `prepare`.
pub const MIGRATION_FAILED: &str = "effect_hold_pg.migration_failed";

/// A database failure while running a statement, as a ledger error.
pub(crate) fn pg(operation: &str, error: &PgError) -> Error {
    unavailable(format!("{operation}: {error}"))
}

/// A pool failure (no connection could be checked out), as a ledger error.
pub(crate) fn pool(operation: &str, error: &PoolError) -> Error {
    unavailable(format!("{operation}: no connection: {error}"))
}

/// A stored row that cannot be read back (unknown state text, a negative id, unreadable json).
pub(crate) fn corrupt(what: String) -> Error {
    unavailable(format!("a stored row is not readable: {what}"))
}

/// The ledger could not complete the call, so the site must be treated as held.
fn unavailable(reason: String) -> Error {
    Error::LedgerUnavailable { reason }
}

/// A failed schema migration, as the `effect_hold_pg.migration_failed` code.
pub(crate) fn migration(error: &PgError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!("the side-effect hold ledger schema could not be created: {error}"),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(error: &PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!("the side-effect hold ledger schema could not be created: no connection: {error}"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_is_the_fail_closed_error() {
        let error = corrupt("an unknown status".to_owned());
        assert!(matches!(error, Error::LedgerUnavailable { .. }));
        assert_eq!(
            error.code(),
            spot_effect_hold::codes::LEDGER_UNAVAILABLE,
            "a broken ledger is reported as the code the hold treats as held"
        );
    }

    #[test]
    fn the_migration_code_is_the_catalogued_one() {
        assert_eq!(MIGRATION_FAILED, "effect_hold_pg.migration_failed");
    }
}
