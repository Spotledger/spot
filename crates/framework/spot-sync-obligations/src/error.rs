//! The one error type of this crate, with stable `sync_obligations.*` codes.
//!
//! Messages carry positions and field names only, never record contents.

/// A setting or an input the ledger arithmetic cannot use.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ObligationError {
    /// An [`ObligationSettings`](crate::ObligationSettings) field is out of range.
    #[error("invalid obligation setting: {0}")]
    SettingsInvalid(&'static str),

    /// A hold names a stream position that is not among the mandatory events given.
    #[error("hold at position {0} matches no mandatory event")]
    HoldUnknown(u64),

    /// The applied prefix moved backwards: {before} then {after}.
    #[error("applied prefix moved backwards from {before} to {after}")]
    PrefixRegressed {
        /// The earlier prefix.
        before: u64,
        /// The later prefix, lower than `before`.
        after: u64,
    },
}

impl ObligationError {
    /// The stable machine-readable code, for logs, API responses and metrics.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SettingsInvalid(_) => SETTINGS_INVALID,
            Self::HoldUnknown(_) => HOLD_UNKNOWN,
            Self::PrefixRegressed { .. } => PREFIX_REGRESSED,
        }
    }
}

/// A setting is out of range.
pub const SETTINGS_INVALID: &str = "sync_obligations.settings_invalid";
/// A hold names a position that is not a known mandatory event.
pub const HOLD_UNKNOWN: &str = "sync_obligations.hold_unknown";
/// The applied prefix went backwards.
pub const PREFIX_REGRESSED: &str = "sync_obligations.prefix_regressed";
