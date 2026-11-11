//! Errors: the trait crate's own [`EnvironmentError`], with a fixed sentence and only a code
//! (the bridge's or the database's SQLSTATE) as the variable part.

use spot_environments::{EnvironmentError, codes as trait_codes};
use spot_pg_bridge::BridgeError;

/// The codes this crate adds to the trait crate's `environments.*` vocabulary.
pub mod codes {
    /// The stored document of a set does not describe a valid set.
    pub const ROW_INVALID: &str = "environments_pg.row_invalid";
    /// The schema could not be prepared, or the configuration cannot hold.
    pub const SETUP_FAILED: &str = "environments_pg.setup_failed";
}

/// What a failure says without any value: the bridge code, or the SQLSTATE of the database.
pub(crate) fn cause(error: &BridgeError<tokio_postgres::Error>) -> String {
    match error {
        BridgeError::Store(e) => e.code().map_or_else(
            || "the database call failed".to_owned(),
            |state| format!("SQLSTATE {}", state.code()),
        ),
        other => other
            .code()
            .map_or_else(|| "the call failed".to_owned(), str::to_owned),
    }
}

/// `environments.store_failed` for a bridge refusal or a database error.
pub(crate) fn store_failed(error: &BridgeError<tokio_postgres::Error>) -> EnvironmentError {
    EnvironmentError::new(
        trait_codes::STORE_FAILED,
        format!(
            "the environment store could not be read or written ({})",
            cause(error)
        ),
    )
}

/// `environments.store_failed` for a store-side limit (the list ceiling).
pub(crate) fn limit(message: &'static str) -> EnvironmentError {
    EnvironmentError::new(trait_codes::STORE_FAILED, message)
}

/// `environments_pg.row_invalid`: a stored document is not a valid set. Names no content.
pub(crate) fn row_invalid() -> EnvironmentError {
    EnvironmentError::new(
        codes::ROW_INVALID,
        "a stored environment set is not a valid set",
    )
}

/// `environments_pg.setup_failed`.
pub(crate) fn setup(message: impl Into<String>) -> EnvironmentError {
    EnvironmentError::new(codes::SETUP_FAILED, message)
}
