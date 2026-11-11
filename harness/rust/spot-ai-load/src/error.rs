//! The one error type of the harness.

use std::fmt;

/// Why a run could not be carried out (not a budget breach: those are in the report).
#[derive(Debug)]
pub enum HarnessError {
    /// A database call failed; the text names the step, never a credential.
    Database(String),
    /// The admission crate refused a setting the harness gave it.
    Admission(String),
    /// A configuration or budgets file did not parse or does not make sense.
    Config(String),
    /// A spawned task panicked or was aborted.
    Task(String),
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(m) => write!(f, "database: {m}"),
            Self::Admission(m) => write!(f, "admission: {m}"),
            Self::Config(m) => write!(f, "config: {m}"),
            Self::Task(m) => write!(f, "task: {m}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl HarnessError {
    /// A database error at `step`. Only the server's message is kept, which carries no secret.
    pub(crate) fn db(step: &str, error: &tokio_postgres::Error) -> Self {
        let detail = error.as_db_error().map_or_else(
            || "connection or protocol error".to_owned(),
            |e| e.message().to_owned(),
        );
        Self::Database(format!("{step}: {detail}"))
    }
}

impl From<spot_ai_isolation::AiError> for HarnessError {
    fn from(error: spot_ai_isolation::AiError) -> Self {
        Self::Admission(error.to_string())
    }
}

/// The result of every fallible harness operation.
pub type Result<T> = std::result::Result<T, HarnessError>;
