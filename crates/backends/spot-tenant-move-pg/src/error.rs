//! Errors: the constructor's refusal and the mapping of every failure of a call onto the
//! trait's own [`Error::StoreFailed`] with a fixed reason.
//!
//! No reason carries SQL, a parameter, a connection string, a driver message or a row value:
//! they are `&'static str` chosen here. The trait is synchronous and its error has no source
//! slot, so the code of a bridge refusal (`pg_bridge.busy` and the others) is written into the
//! fixed reason text instead.

use std::fmt;

use spot_pg_bridge::BridgeError;
use spot_tenant_move::Error;

/// Stable code: the store could not be built (bridge setting, current-thread runtime).
pub const BAD_SETUP: &str = "tenant_move_pg.bad_setup";
/// Stable code: the control tables could not be created.
pub const MIGRATION_FAILED: &str = "tenant_move_pg.migration_failed";

/// Why a [`crate::PgMoveStore`] could not be built or prepared. The message is a fixed sentence.
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

/// The store failure for a database error inside a call: a fixed reason, the SQLSTATE class is
/// deliberately not reported.
pub(crate) fn query_failed(tenant: &str) -> Error {
    store_failed(tenant, "the move store query failed")
}

pub(crate) fn store_failed(tenant: &str, reason: &'static str) -> Error {
    Error::StoreFailed {
        tenant: tenant.to_owned(),
        reason,
    }
}

/// Whether a driver error is a unique-constraint violation (`SQLSTATE 23505`).
pub(crate) fn is_unique_violation(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|db| db.code().code() == "23505")
}

/// Maps the bridge's answer onto the trait's error. A refusal is never a default value.
pub(crate) fn from_bridge<T>(
    tenant: &str,
    result: Result<T, BridgeError<Error>>,
) -> Result<T, Error> {
    result.map_err(|error| match error {
        BridgeError::Store(inner) => inner,
        BridgeError::WrongThread => store_failed(
            tenant,
            "pg_bridge.wrong_thread: the move store was called from an async thread",
        ),
        BridgeError::Busy => store_failed(tenant, "pg_bridge.busy: the move store is busy"),
        BridgeError::Timeout => store_failed(
            tenant,
            "pg_bridge.timeout: the move store call missed its deadline",
        ),
        BridgeError::Closed => store_failed(tenant, "pg_bridge.closed: the bridge is shut down"),
        BridgeError::Connect => store_failed(
            tenant,
            "pg_bridge.connect: no database connection is available",
        ),
        _ => store_failed(tenant, "pg_bridge.config: the bridge refused the call"),
    })
}
