//! Typed errors with stable `journal_recovery.*` codes.

/// An input the report needs could not be read. The report records this as an unreadable fact and
/// the verdict becomes "cannot confirm"; it is never read as "fine".
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the recovery input {input} could not be read")]
pub struct FactUnreadable {
    /// Which input (a stable key such as `key_backup`), never a value.
    pub input: &'static str,
}

impl FactUnreadable {
    /// An unreadable input.
    #[must_use]
    pub fn new(input: &'static str) -> Self {
        Self { input }
    }

    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        "journal_recovery.fact_unreadable"
    }
}
