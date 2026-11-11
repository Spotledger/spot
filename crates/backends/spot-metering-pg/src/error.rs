//! Turning database and pool failures into `MeteringError`, plus the one code this backend
//! adds to the catalogue.
//!
//! A database or pool failure is reported as `MeteringError::StoreUnavailable` with a stable
//! short cause code (`pool_exhausted`, `connection_lost`, `timeout`, `conflict`,
//! `statement_failed`, `bad_data`): never the SQLSTATE, a host, a value or driver text, because
//! the driver's own message can quote values and constraint detail, and a metering error is
//! logged and enveloped. `InvalidPeriodSpec` is left to real period specifications. A counter
//! sum that would exceed `u64` (the `spot_meter_total_sum_range` constraint) is the exact
//! `CounterOverflow` the in-memory store reports, and the whole batch is rolled back.

use deadpool_postgres::{PoolError, TimeoutType};
use spot_app_api::{AppError, ErrorClass};
use spot_metering::MeteringError;

use crate::schema::SUM_RANGE_CONSTRAINT;

/// The metering tables could not be created: a `metering_pg.*` code raised by `prepare`.
pub const MIGRATION_FAILED: &str = "metering_pg.migration_failed";

/// The SQLSTATE of a driver error, or `none` for a connection-level failure.
fn sqlstate(error: &tokio_postgres::Error) -> &str {
    error.code().map_or("none", |state| state.code())
}

/// Whether the failed transaction may simply be run again (deadlock or serialisation failure).
pub(crate) fn is_retryable(error: &tokio_postgres::Error) -> bool {
    matches!(sqlstate(error), "40P01" | "40001")
}

/// A database failure while running a statement.
pub(crate) fn pg(error: &tokio_postgres::Error) -> MeteringError {
    // 22003 is `numeric field overflow`: a batch whose own sum already exceeds `numeric(20,0)`
    // (about 10^20, above `u64::MAX`) fails before the range constraint is checked.
    if sqlstate(error) == "22003"
        || error
            .as_db_error()
            .is_some_and(|db| db.constraint() == Some(SUM_RANGE_CONSTRAINT))
    {
        return MeteringError::CounterOverflow;
    }
    unavailable(match sqlstate(error) {
        "none" => "connection_lost",
        "40P01" | "40001" => "conflict",
        "57014" => "timeout",
        state if state.starts_with("08") || state.starts_with("57P") => "connection_lost",
        _ => "statement_failed",
    })
}

/// A pool failure (no connection could be checked out).
pub(crate) fn pool(error: PoolError) -> MeteringError {
    unavailable(match error {
        PoolError::Timeout(TimeoutType::Wait) => "pool_exhausted",
        PoolError::Timeout(_) => "timeout",
        PoolError::Backend(_) | PoolError::PostCreateHook(_) => "connection_lost",
        PoolError::Closed | PoolError::NoRuntimeSpecified => "pool_closed",
    })
}

/// A stored row that cannot be read back.
pub(crate) fn corrupt(_what: &str) -> MeteringError {
    unavailable("bad_data")
}

/// A transaction that kept conflicting until the retries were spent.
pub(crate) fn conflicting() -> MeteringError {
    unavailable("conflict")
}

fn unavailable(cause: &'static str) -> MeteringError {
    MeteringError::StoreUnavailable { cause }
}

/// A failed schema migration, as the `metering_pg.migration_failed` code.
pub(crate) fn migration(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the metering tables could not be created (SQLSTATE {})",
            sqlstate(&error)
        ),
    )
}

/// A pool failure while preparing the schema.
pub(crate) fn migration_pool(_: PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the metering tables could not be created: no connection".to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_failures_map_to_store_unavailable_with_a_cause_code_only() {
        for (error, cause) in [
            (PoolError::Timeout(TimeoutType::Wait), "pool_exhausted"),
            (PoolError::Timeout(TimeoutType::Create), "timeout"),
            (PoolError::Timeout(TimeoutType::Recycle), "timeout"),
            (PoolError::Closed, "pool_closed"),
        ] {
            let mapped = pool(error);
            assert_eq!(mapped, MeteringError::StoreUnavailable { cause });
            assert_eq!(mapped.code(), "metering.store_unavailable");
        }
    }

    #[test]
    fn the_unavailable_text_has_no_sqlstate_or_driver_wording() {
        for mapped in [corrupt("row count"), conflicting()] {
            let text = mapped.to_string();
            assert!(
                text.starts_with("the metering store is unavailable ("),
                "{text}"
            );
            assert!(!text.contains("SQLSTATE"), "{text}");
        }
    }
}
