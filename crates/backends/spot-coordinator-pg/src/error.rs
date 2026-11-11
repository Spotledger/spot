//! Turning database and pool failures into the stable `coordinator.*` errors of the contract.
//!
//! Every code in the coordinator family is catalogued in `spot-errors`; this backend raises one
//! it adds, `coordinator.unavailable`, when the database or its pool could not complete a call.
//! Callers match on [`Error::code`](spot_coordinator::Error), never on a message.

use deadpool_postgres::PoolError;
use spot_coordinator::Error;
use tokio_postgres::Error as PgError;

/// The database or its connection pool could not complete the call: a connection that could not
/// be opened, a statement that failed, or a pool that timed out.
pub const UNAVAILABLE: &str = "coordinator.unavailable";

/// A database failure, as a coordinator error.
pub(crate) fn pg(what: &str, error: &PgError) -> Error {
    Error::new(UNAVAILABLE, format!("{what}: {}", clean(error)))
}

/// A pool failure, as a coordinator error.
pub(crate) fn pool(what: &str, error: &PoolError) -> Error {
    Error::new(UNAVAILABLE, format!("{what}: {error}"))
}

/// The database's own message; `tokio-postgres`'s `Display` already includes any server detail.
fn clean(error: &PgError) -> String {
    error.to_string()
}
