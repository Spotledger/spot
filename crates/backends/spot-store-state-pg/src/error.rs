//! Errors: the constructor's refusals and the mapping of every failure of a call onto the port's
//! own `store_state.*` refusals.
//!
//! A database or bridge failure is `store_state.backend` with a fixed sentence, never a driver
//! message, SQL, parameter, connection string or row value. The code of a bridge refusal
//! (`pg_bridge.busy` and the others) is written into the sentence.

use std::fmt;

use spot_pg_bridge::BridgeError;
use spot_store_state::StoreStateError;

/// Stable code: the store could not be built (bridge setting, current-thread runtime).
pub const BAD_SETUP: &str = "store_state.bad_setup";
/// Stable code: the tables could not be created.
pub const MIGRATION_FAILED: &str = "store_state.migration_failed";

/// Why a [`crate::PgStoreState`] could not be built or prepared. The message is a fixed sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupError {
    code: &'static str,
    reason: &'static str,
}

impl SetupError {
    pub(crate) fn bad_setup(reason: &'static str) -> Self {
        Self {
            code: BAD_SETUP,
            reason,
        }
    }

    pub(crate) fn migration(reason: &'static str) -> Self {
        Self {
            code: MIGRATION_FAILED,
            reason,
        }
    }

    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.reason)
    }
}

impl std::error::Error for SetupError {}

/// The failure for a driver error on a read or write.
pub(crate) fn query_failed<E>(_error: E) -> StoreStateError {
    StoreStateError::backend("the store state query failed")
}

/// A stored row that cannot be turned back into a record.
pub(crate) fn corrupt() -> StoreStateError {
    StoreStateError::backend("a stored store state record cannot be read")
}

/// The constraint name of a unique violation (`SQLSTATE 23505`), if the error is one.
pub(crate) fn unique_violation(error: &tokio_postgres::Error) -> Option<String> {
    error
        .as_db_error()
        .filter(|db| db.code().code() == "23505")
        .map(|db| db.constraint().unwrap_or_default().to_owned())
}

/// Whether the error is a foreign-key violation (`SQLSTATE 23503`).
pub(crate) fn is_foreign_key_violation(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|db| db.code().code() == "23503")
}

/// Maps the bridge's answer onto the port's error. A refusal is never a default value.
pub(crate) fn from_bridge<T>(
    result: Result<T, BridgeError<StoreStateError>>,
) -> Result<T, StoreStateError> {
    result.map_err(|error| match error {
        BridgeError::Store(inner) => inner,
        BridgeError::WrongThread => StoreStateError::backend(
            "pg_bridge.wrong_thread: the store state was called from an async thread",
        ),
        BridgeError::Busy => StoreStateError::backend("pg_bridge.busy: the store state is busy"),
        BridgeError::Timeout => {
            StoreStateError::backend("pg_bridge.timeout: the store state call missed its deadline")
        }
        BridgeError::Closed => {
            StoreStateError::backend("pg_bridge.closed: the bridge is shut down")
        }
        BridgeError::Connect => {
            StoreStateError::backend("pg_bridge.connect: no database connection is available")
        }
        _ => StoreStateError::backend("pg_bridge.config: the bridge refused the call"),
    })
}
