//! Errors: the constructor's refusals and the mapping of every failure of a call onto the port's
//! own [`KeyStoreError`].
//!
//! A database or bridge failure is `protected_keys_pg.store_failed` with a fixed sentence that
//! carries the store name and, for a database error, the SQLSTATE *class* (the first two
//! characters) only: never a driver message, SQL, parameter, connection string, row value or key.

use std::fmt;

use spot_pg_bridge::BridgeError;
use spot_protected_keys::KeyStoreError;

/// Stable code: the store could not be built (tenant name, bridge setting).
pub const BAD_SETUP: &str = "protected_keys_pg.bad_setup";
/// Stable code: the key tables could not be created.
pub const MIGRATION_FAILED: &str = "protected_keys_pg.migration_failed";

/// Why a [`crate::PgKeyStore`] could not be built or prepared. The message is a fixed sentence.
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
const STORE: &str = "pg key store";

/// A `Failed` with a fixed sentence.
pub(crate) fn failed(reason: &str) -> KeyStoreError {
    KeyStoreError::Failed(format!("{STORE}: {reason}"))
}

/// A database failure: the SQLSTATE class only (`none` for a failure with no server code, such
/// as a closed connection).
pub(crate) fn pg(error: &tokio_postgres::Error) -> KeyStoreError {
    let class = error
        .as_db_error()
        .and_then(|db| db.code().code().get(..2))
        .unwrap_or("none");
    KeyStoreError::Failed(format!("{STORE}: database error (sqlstate class {class})"))
}

/// Maps the bridge's answer onto the port's error. A refusal is never a default value.
pub(crate) fn from_bridge<T>(
    result: Result<T, BridgeError<KeyStoreError>>,
) -> Result<T, KeyStoreError> {
    match result {
        Ok(value) => Ok(value),
        Err(BridgeError::Store(inner)) => Err(inner),
        Err(BridgeError::WrongThread) => Err(failed(
            "pg_bridge.wrong_thread: the key store was called from a thread inside an async \
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
