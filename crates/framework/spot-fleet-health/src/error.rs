//! The crate's error type and its stable `fleet_health.*` codes.
//!
//! Every refusal carries a code from the error catalogue
//! (`crates/framework/spot-errors/catalogue/fleet_health.toml`). Messages name the field or the
//! reason, never a secret or a raw payload.

use thiserror::Error;

/// A refusal from the fleet health engine: malformed input, an invalid rule set, or a silence
/// that cannot be honoured.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FleetHealthError {
    /// A tenant or node identifier is empty, too long or contains a forbidden character.
    #[error(
        "a {kind} identifier must be 1 to 128 characters of letters, digits, '-', '_', '.' or ':'"
    )]
    InvalidIdentifier {
        /// Which kind of identifier was refused (`tenant` or `node`).
        kind: &'static str,
    },

    /// The evaluation cadence or window is inconsistent.
    #[error("the fleet health settings are invalid: {reason}")]
    InvalidSettings {
        /// What is wrong with the settings.
        reason: String,
    },

    /// The rule set could not be parsed or violates a rule invariant.
    #[error("the fleet health rule set is invalid: {reason}")]
    InvalidRuleSet {
        /// What is wrong with the rule set.
        reason: String,
    },

    /// An observation's value is outside the signal's domain.
    #[error("a fleet health observation is invalid: {reason}")]
    InvalidObservation {
        /// What is wrong with the observation.
        reason: String,
    },

    /// A silence has no reason or expires in the past.
    #[error("a fleet health silence is invalid: {reason}")]
    InvalidSilence {
        /// What is wrong with the silence.
        reason: String,
    },
}

impl FleetHealthError {
    /// The stable catalogue code for this refusal.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidIdentifier { .. } => "fleet_health.invalid_identifier",
            Self::InvalidSettings { .. } => "fleet_health.invalid_settings",
            Self::InvalidRuleSet { .. } => "fleet_health.invalid_rule_set",
            Self::InvalidObservation { .. } => "fleet_health.invalid_observation",
            Self::InvalidSilence { .. } => "fleet_health.invalid_silence",
        }
    }
}
