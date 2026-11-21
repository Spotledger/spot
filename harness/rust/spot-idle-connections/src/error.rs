//! The harness's error: a plain message, since a failed run is reported, not recovered from.

use std::fmt;

/// Why a run could not be carried out (not a budget miss: those are in the report).
#[derive(Debug)]
pub struct HarnessError(pub String);

impl HarnessError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub(crate) fn io(context: &str, error: &std::io::Error) -> Self {
        Self(format!("{context}: {error}"))
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HarnessError {}
