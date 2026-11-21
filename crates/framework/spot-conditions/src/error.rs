//! Typed errors with stable `conditions.*` codes.
//!
//! Callers match on [`ConditionsError::code`], never on the message. Every code is catalogued in
//! `crates/framework/spot-errors/catalogue/conditions.toml`.

/// What can go wrong when a condition is registered or a transition is asked for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConditionsError {
    /// No condition is known under the id.
    ///
    /// Covers both refusals that say "this id is not a condition": an id that is not upper snake
    /// case (so it could never have been registered) and a well-formed id that is not in the
    /// registry.
    #[error("no registered condition has the id {id}")]
    UnknownId {
        /// The id as the caller spelled it.
        id: String,
    },

    /// A condition with this id is already registered.
    ///
    /// Ids are stable names (ADR-067 section 1), so a second definition of one is a mistake, not
    /// an update: it would silently change the severity, response or escalation of every subject
    /// that already has the condition open.
    #[error("a condition with the id {id} is already registered")]
    DuplicateId {
        /// The id as the caller spelled it.
        id: String,
    },
}

impl ConditionsError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownId { .. } => "conditions.unknown_id",
            Self::DuplicateId { .. } => "conditions.duplicate_id",
        }
    }
}
