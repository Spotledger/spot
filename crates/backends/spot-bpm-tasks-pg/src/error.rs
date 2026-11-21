//! Errors: the constructor's refusals and the mapping of every failure of a call onto the
//! trait's own [`TasksError`].
//!
//! A database or bridge failure is `bpm_tasks.store_failed` with a fixed sentence that carries
//! the store name and, for a database error, the SQLSTATE *class* (the first two characters)
//! only: never a driver message, SQL, parameter, connection string, row value, comment or key.

use std::fmt;

use spot_bpm_tasks::TasksError;
use spot_pg_bridge::BridgeError;

/// Stable code: the store could not be built (tenant name, bridge setting).
pub const BAD_SETUP: &str = "bpm_tasks_pg.bad_setup";
/// Stable code: the task tables could not be created.
pub const MIGRATION_FAILED: &str = "bpm_tasks_pg.migration_failed";

/// Why a [`crate::PgTaskStore`] could not be built or prepared. The message is a fixed sentence.
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

/// The name every failure message starts with.
const STORE: &str = "pg task store";

/// A `StoreFailed` with a fixed sentence.
pub(crate) fn failed(reason: &str) -> TasksError {
    TasksError::StoreFailed {
        detail: format!("{STORE}: {reason}"),
    }
}

/// A database failure: the SQLSTATE class only (`none` for a failure with no server code, such
/// as a closed connection).
pub(crate) fn pg(error: &tokio_postgres::Error) -> TasksError {
    let class = error
        .as_db_error()
        .and_then(|db| db.code().code().get(..2))
        .unwrap_or("none");
    TasksError::StoreFailed {
        detail: format!("{STORE}: database error (sqlstate class {class})"),
    }
}

/// A stored row that cannot be read back, or that disagrees with its own columns.
pub(crate) fn corrupt(what: &str) -> TasksError {
    failed(&format!("a stored row is not readable ({what})"))
}

/// Maps the bridge's answer onto the trait's error. A refusal is never a default value.
pub(crate) fn from_bridge<T>(result: Result<T, BridgeError<TasksError>>) -> Result<T, TasksError> {
    match result {
        Ok(value) => Ok(value),
        Err(BridgeError::Store(inner)) => Err(inner),
        Err(BridgeError::WrongThread) => Err(failed(
            "pg_bridge.wrong_thread: the task store was called from a thread inside an async \
             runtime; call it from a plain thread outside any tokio runtime",
        )),
        Err(BridgeError::Busy) => Err(failed("pg_bridge.busy: the bridge queue is full")),
        Err(BridgeError::Timeout) => Err(failed(
            "pg_bridge.timeout: the call missed its deadline; a write may or may not have applied",
        )),
        Err(BridgeError::Closed) => Err(failed("pg_bridge.closed: the bridge is shut down")),
        Err(BridgeError::Connect) => Err(failed(
            "pg_bridge.connect: no database connection is available",
        )),
        Err(_) => Err(failed("pg_bridge.config: the bridge refused the call")),
    }
}
