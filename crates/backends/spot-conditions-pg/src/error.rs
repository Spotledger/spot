//! Typed errors with stable `conditions_pg.*` codes.
//!
//! Every variant is catalogued in
//! `crates/framework/spot-errors/catalogue/conditions_pg.toml`. The message never carries a
//! parameter value, a row or SQL text: only a fixed sentence, so nothing a condition's
//! parameters hold (ADR-067 section 1 forbids a record's content there in the first place) can
//! ever reach a log line through this error.

/// What can go wrong when the durable condition store is prepared or used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConditionsPgError {
    /// `prepare` could not create the tables.
    #[error("the condition store's tables could not be created")]
    MigrationFailed,
    /// A read or a write could not be completed: no connection, a failed statement, or a
    /// transaction that was poisoned or aborted. Fail closed: the caller never guesses whether
    /// the write landed.
    #[error("the condition store could not complete the operation")]
    Unavailable,
    /// A stored row does not decode: an id or a severity this build does not know, or params
    /// that are not a flat object of text values. The store refuses to adopt it rather than
    /// guess its meaning.
    #[error("a stored condition row could not be decoded")]
    Corrupt,
}

impl ConditionsPgError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MigrationFailed => "conditions_pg.migration_failed",
            Self::Unavailable => "conditions_pg.unavailable",
            Self::Corrupt => "conditions_pg.corrupt",
        }
    }
}
