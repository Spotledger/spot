//! Error vocabulary and stable catalogue codes (`catalogue/sync_repair_pull.toml`).

use thiserror::Error;

/// A missing key lies outside the node's subscription scope: the hub would refuse the ask.
pub const OUT_OF_SCOPE: &str = "sync_repair_pull.out_of_scope";
/// A repair setting is zero or inconsistent.
pub const INVALID_CONFIG: &str = "sync_repair_pull.invalid_config";

/// Why a [`crate::RepairConfig`] cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepairError {
    /// A setting is zero or inconsistent with another.
    #[error("The repair settings are not usable: {reason}.")]
    InvalidConfig {
        /// Which rule the settings break.
        reason: &'static str,
    },
}

impl RepairError {
    /// The stable dotted catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfig { .. } => INVALID_CONFIG,
        }
    }
}
