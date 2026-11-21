//! Errors: the constructor's refusals and the mapping of every failure of a call onto the
//! trait's own [`ConversationError`].
//!
//! `ConversationError::Store` carries no text, so a failed call reveals nothing at all about the
//! statement, the row or the server: not a driver message, SQL, parameter, key or SQLSTATE. The
//! SQLSTATE *class* is available to the host from the bridge's metrics and the server's own log;
//! nothing here logs.

use std::fmt;

use spot_bpm_conversations::ConversationError;
use spot_pg_bridge::BridgeError;

/// Stable code: the store could not be built (tenant name, bridge setting).
pub const BAD_SETUP: &str = "bpm_conversations_pg.bad_setup";
/// Stable code: the conversation tables could not be created.
pub const MIGRATION_FAILED: &str = "bpm_conversations_pg.migration_failed";

/// Why a [`crate::PgConversationStore`] could not be built or prepared. The message is a fixed
/// sentence.
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

/// A database failure: `bpm_conversations.store_failed`, with nothing else. The error type has
/// no field for text, which is the point.
pub(crate) fn pg(_error: &tokio_postgres::Error) -> ConversationError {
    ConversationError::Store
}

/// A stored row that cannot be read back, or that disagrees with its own columns.
pub(crate) fn corrupt() -> ConversationError {
    ConversationError::Store
}

/// Maps the bridge's answer onto the trait's error. A refusal is never a default value, and a
/// call from inside an async runtime is refused before anything runs.
pub(crate) fn from_bridge<T>(
    result: Result<T, BridgeError<ConversationError>>,
) -> Result<T, ConversationError> {
    match result {
        Ok(value) => Ok(value),
        Err(BridgeError::Store(inner)) => Err(inner),
        Err(_) => Err(ConversationError::Store),
    }
}
