//! Typed errors with stable [`codes`](crate::codes).
//!
//! Messages name the operation and, for a database failure, its SQLSTATE and the server's
//! primary message. They never carry a connection string, a password or a raw SQL statement.

use crate::codes;

/// Anything the PostgreSQL wiring can refuse to do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The control schema, one of its tables or a row could not be read or written.
    #[error("{}: {operation}: {message}", codes::CONTROL_UNAVAILABLE)]
    ControlUnavailable {
        /// The operation that failed (`reading the journal`, `closing the gate`, ...).
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
        /// The site the journal belongs to.
        site: String,
        /// The sequence number offered.
        seq: u64,
        /// The sequence number the journal expected.
        expected: u64,
    },
    /// A stored journal record does not parse back into a record.
    #[error(
        "{}: journal record {seq} does not parse: {message}",
        codes::JOURNAL_RECORD
    )]
    JournalRecord {
        /// The record's sequence number.
        seq: u64,
        /// Why it does not parse.
        message: String,
    },
    /// The gate row could not be read, so the gate is closed.
    #[error("{}: {message}", codes::GATE_UNREADABLE)]
    GateUnreadable {
        /// What could not be read (never a SQL statement).
        message: String,
    },
    /// A step's index is outside the plan the runner was prepared for.
    #[error(
        "{}: step {index} is outside the {steps}-step plan",
        codes::STEP_OUT_OF_PLAN
    )]
    StepOutOfPlan {
        /// The step's index.
        index: usize,
        /// How many steps the plan has.
        steps: usize,
    },
    /// A schema step failed.
    #[error("{}: step {index} ({id}): {message}", codes::STEP_FAILED)]
    StepFailed {
        /// The step's index.
        index: usize,
        /// The step's id.
        id: String,
        /// What `spot-pg-migrate` reported.
        message: String,
    },
    /// A non-schema step has no handler wired in this build.
    #[error(
        "{}: the non-schema step {kind} has no handler in this build",
        codes::STEP_UNSUPPORTED
    )]
    StepUnsupported {
        /// The step kind that needs a handler.
        kind: String,
    },
    /// The snapshot provider failed.
    #[error("{}: {operation}: {message}", codes::SNAPSHOT_FAILED)]
    SnapshotFailed {
        /// The operation that failed (`take`, `restore`, `verify`).
        operation: String,
        /// What the provider reported.
        message: String,
    },
    /// The snapshot id is not one this port took on this site.
    #[error("{}: {snapshot}", codes::SNAPSHOT_UNKNOWN)]
    SnapshotUnknown {
        /// The unknown snapshot id.
        snapshot: String,
    },
    /// A timing measurement could not be recorded.
    #[error("{}: {message}", codes::TIMING_FAILED)]
    TimingFailed {
        /// What failed.
        message: String,
    },
    /// A `migration.progress` notification could not be published.
    #[error("{}: {message}", codes::PROGRESS_FAILED)]
    ProgressFailed {
        /// What failed.
        message: String,
    },
    /// The tenant room could not be built.
    #[error("{}: {message}", codes::PROGRESS_ROOM)]
    ProgressRoom {
        /// Why the room name is invalid.
        message: String,
    },
}

impl Error {
    /// The stable catalogue code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ControlUnavailable { .. } => codes::CONTROL_UNAVAILABLE,
            Self::JournalSequence { .. } => codes::JOURNAL_SEQUENCE,
            Self::JournalRecord { .. } => codes::JOURNAL_RECORD,
            Self::GateUnreadable { .. } => codes::GATE_UNREADABLE,
            Self::StepOutOfPlan { .. } => codes::STEP_OUT_OF_PLAN,
            Self::StepFailed { .. } => codes::STEP_FAILED,
            Self::StepUnsupported { .. } => codes::STEP_UNSUPPORTED,
            Self::SnapshotFailed { .. } => codes::SNAPSHOT_FAILED,
            Self::SnapshotUnknown { .. } => codes::SNAPSHOT_UNKNOWN,
            Self::TimingFailed { .. } => codes::TIMING_FAILED,
            Self::ProgressFailed { .. } => codes::PROGRESS_FAILED,
            Self::ProgressRoom { .. } => codes::PROGRESS_ROOM,
        }
    }

    /// The failure for a database error, keeping only the SQLSTATE and the primary message.
    pub(crate) fn database(operation: &str, error: &tokio_postgres::Error) -> Self {
        Self::ControlUnavailable {
            operation: operation.to_owned(),
            message: sqlstate_message(error),
        }
    }
}

/// The SQLSTATE and primary message of a driver error: never the statement or its parameters.
pub(crate) fn sqlstate_message(error: &tokio_postgres::Error) -> String {
    let state = error.code().map_or("-----", |code| code.code());
    let message = error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_owned());
    format!("{state} {message}")
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
