//! Errors: the trait crate's own [`FixError::JournalFailed`], with a fixed sentence and only a
//! code (the bridge's, or the database's SQLSTATE) as the variable part. Never SQL, a
//! connection string or a row.

use std::fmt;

use spot_fix_pipeline::FixError;
use spot_pg_bridge::BridgeError;

/// The codes this crate adds to the `fix_pipeline.*` vocabulary.
pub mod codes {
    /// The journal could not be set up: the bridge configuration does not fit the pool, the
    /// runtime is current-thread, or the schema could not be prepared.
    pub const SETUP_FAILED: &str = "fix_pipeline_pg.setup_failed";
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

/// `fix_pipeline.journal_failed` for a bridge refusal or a database error.
pub(crate) fn journal_failed(what: &str, error: &BridgeError<tokio_postgres::Error>) -> FixError {
    FixError::JournalFailed {
        detail: format!("{what} ({})", cause(error)),
    }
}

/// `fix_pipeline.journal_failed` with a fixed sentence (a decode failure, a store-side limit).
pub(crate) fn journal_message(message: impl Into<String>) -> FixError {
    FixError::JournalFailed {
        detail: message.into(),
    }
}

/// The journal could not be set up (`fix_pipeline_pg.setup_failed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupError {
    message: String,
}

impl SetupError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The stable code, `fix_pipeline_pg.setup_failed`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        codes::SETUP_FAILED
    }

    /// The fixed sentence, with at most a bridge code or SQLSTATE in it.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", codes::SETUP_FAILED, self.message)
    }
}

impl std::error::Error for SetupError {}
