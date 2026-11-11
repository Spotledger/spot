//! The catalogue's own validation error.

/// A catalogue entry that breaks one of the catalogue's rules (calm by default, strictly
/// increasing escalation, a response for every red condition, well-formed message keys).
///
/// Only [`crate::validate_catalogue`] raises it; building the registry raises
/// [`spot_conditions::ConditionsError`] as before.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncConditionsError {
    /// An entry breaks a catalogue rule.
    #[error("the catalogue entry {id} breaks the rule {rule}")]
    InvalidEntry {
        /// The condition id.
        id: String,
        /// The rule, as a short stable name (`reason_missing`, `escalation_order`, ...).
        rule: &'static str,
    },
}

impl SyncConditionsError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidEntry { .. } => "sync_conditions.invalid_entry",
        }
    }
}
