//! Errors: the constructor's refusals and the mapping of every failure of a call onto the trait's
//! own `registry.*` refusals.
//!
//! A database or bridge failure is `registry.storage` with a fixed sentence, never a driver
//! message, SQL, parameter, connection string or row value. The code of a bridge refusal
//! (`pg_bridge.busy` and the others) is written into the sentence, because `RegistryError` has no
//! source slot.

use std::fmt;

use spot_pg_bridge::BridgeError;
use spot_registry::RegistryError;

/// Stable code: the store could not be built (bridge setting, current-thread runtime).
pub const BAD_SETUP: &str = "registry_pg.bad_setup";
/// Stable code: the registry tables could not be created.
pub const MIGRATION_FAILED: &str = "registry_pg.migration_failed";

/// Why a [`crate::PgIndexStore`] could not be built or prepared. The message is a fixed sentence.
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

/// The registry's own storage failure code.
pub const STORAGE: &str = "registry.storage";

/// A storage failure with a fixed sentence.
pub(crate) fn storage(reason: &'static str) -> RegistryError {
    RegistryError::new(STORAGE, reason)
}

/// The failure for a driver error on a read or write.
pub(crate) fn query_failed<E>(_error: E) -> RegistryError {
    storage("the registry index query failed")
}

/// A stored row that cannot be turned back into a release, transfer or ownership.
pub(crate) fn corrupt() -> RegistryError {
    storage("the stored registry record cannot be read")
}

/// Whether a driver error is a unique-constraint violation (`SQLSTATE 23505`).
pub(crate) fn is_unique_violation(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|db| db.code().code() == "23505")
}

/// Maps the bridge's answer onto the trait's error. A refusal is never a default value.
pub(crate) fn from_bridge<T>(
    result: Result<T, BridgeError<RegistryError>>,
) -> Result<T, RegistryError> {
    result.map_err(|error| match error {
        BridgeError::Store(inner) => inner,
        BridgeError::WrongThread => {
            storage("pg_bridge.wrong_thread: the registry index was called from an async thread")
        }
        BridgeError::Busy => storage("pg_bridge.busy: the registry index is busy"),
        BridgeError::Timeout => {
            storage("pg_bridge.timeout: the registry index call missed its deadline")
        }
        BridgeError::Closed => storage("pg_bridge.closed: the bridge is shut down"),
        BridgeError::Connect => storage("pg_bridge.connect: no database connection is available"),
        _ => storage("pg_bridge.config: the bridge refused the call"),
    })
}
