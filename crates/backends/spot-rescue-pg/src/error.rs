//! Typed errors with stable [`codes`](crate::codes). Messages name the operation and, for a
//! database failure, its SQLSTATE and primary message; never a connection string, a statement or a
//! record's content.

use crate::codes;

/// Anything the PostgreSQL rescue journal can refuse to do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The control schema, its table or a row could not be read or written.
    #[error("{}: {operation}: {message}", codes::CONTROL_UNAVAILABLE)]
    ControlUnavailable {
        /// The operation that failed.
        operation: String,
        /// The server's primary message (no SQL, no secret).
        message: String,
    },
    /// The journal was offered a sequence number that is not the next one.
    #[error(
        "{}: sequence {seq} is not the next one ({expected}) for site {site}",
        codes::JOURNAL_SEQUENCE
    )]
    JournalSequence {
        /// The site.
        site: String,
        /// The sequence number offered.
        seq: u64,
        /// The one the journal expected.
        expected: u64,
    },
    /// A stored record does not parse back, or one cannot be stored.
    #[error("{}: journal record {seq}: {message}", codes::JOURNAL_RECORD)]
    JournalRecord {
        /// The record's sequence number.
        seq: u64,
        /// A fixed sentence (never the record).
        message: String,
    },
}

impl Error {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ControlUnavailable { .. } => codes::CONTROL_UNAVAILABLE,
            Self::JournalSequence { .. } => codes::JOURNAL_SEQUENCE,
            Self::JournalRecord { .. } => codes::JOURNAL_RECORD,
        }
    }

    pub(crate) fn database(operation: &str, error: &tokio_postgres::Error) -> Self {
        let state = error.code().map_or("-----", |code| code.code());
        let message = error.as_db_error().map_or_else(
            || "the driver reported an error".to_owned(),
            |db| db.message().to_owned(),
        );
        Self::ControlUnavailable {
            operation: operation.to_owned(),
            message: format!("{state} {message}"),
        }
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
