//! Errors: the setup refusal with its stable code. A failed claim is the port's own
//! [`StoreError::Unavailable`](spot_signed_command::StoreError::Unavailable), which has no field
//! for text, so nothing about the statement, the row or the server can leak.

use std::fmt;

/// Stable code: the nonce table could not be created.
pub const MIGRATION_FAILED: &str = "signed_command_pg.migration_failed";

/// Why [`crate::PgNonceStore::prepare`] failed. The message is a fixed sentence plus, for a
/// statement failure, the SQLSTATE; never a row value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupError {
    reason: String,
}

impl SetupError {
    pub(crate) fn pool() -> Self {
        Self {
            reason: "no connection could be checked out".to_owned(),
        }
    }

    pub(crate) fn statement(error: &tokio_postgres::Error) -> Self {
        let state = error.code().map_or("io", |c| c.code());
        Self {
            reason: format!("the DDL failed (SQLSTATE {state})"),
        }
    }

    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        MIGRATION_FAILED
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{MIGRATION_FAILED}: {}", self.reason)
    }
}

impl std::error::Error for SetupError {}
