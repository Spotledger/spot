//! The one error of the module: a stable code and nothing else.

use spot_bpm_commit::CommitError;
use spot_bpm_effects::{EffectsError, codes};

use super::ledger::LedgerError;

/// A refusal or a failure, as its stable code. No message: a code never carries a value, a
/// secret or text a peer sent, so it is safe to log, return and store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffError(&'static str);

impl EffError {
    /// The request is unknown to the caller: no record file, no such request, or not theirs to
    /// act on. One answer for all three, so the module is not an existence oracle.
    pub const REQUEST_UNKNOWN: Self = Self(codes::REQUEST_UNKNOWN);
    /// An undo (or a decision) is already running for the request.
    pub const REQUEST_LOCKED: Self = Self(codes::REQUEST_LOCKED);
    /// A dependency this call needs is down: the call can be retried.
    pub const UNAVAILABLE: Self = Self(codes::PORT_UNAVAILABLE);
    /// A node name or a receipt is not valid.
    pub const ENTRY_INVALID: Self = Self(codes::ENTRY_INVALID);
    /// The partner module is not present.
    pub const PARTNER_UNAVAILABLE: Self = Self(codes::CANCELLATION_FAILED);

    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for EffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for EffError {}

impl From<EffectsError> for EffError {
    fn from(error: EffectsError) -> Self {
        Self(error.code())
    }
}

impl From<CommitError> for EffError {
    fn from(error: CommitError) -> Self {
        // The hold's own code tells a dispatch failure (retry the commit point) from a state it
        // cannot read; find the matching static constant, or fall back to the commit code.
        use spot_effect_hold::codes as hold;
        let known = [
            hold::ALREADY_HELD,
            hold::NOT_HELD,
            hold::WRONG_MIGRATION,
            hold::STATE_UNKNOWN,
            hold::FISCAL_CANNOT_BE_HELD,
            hold::RELEASE_NEEDS_CONFIRMATION,
            hold::LEDGER_UNAVAILABLE,
            hold::DISPATCH_FAILED,
            hold::NEVER_HOLD_NOT_ALLOWED,
            hold::INVALID_INPUT,
            hold::RELEASE_IN_PROGRESS,
            hold::AUDIT_FAILED,
        ];
        let code = error
            .hold_code()
            .and_then(|code| known.into_iter().find(|known| *known == code))
            .unwrap_or_else(|| error.code());
        Self(code)
    }
}

impl From<spot_effect_hold::Error> for EffError {
    fn from(error: spot_effect_hold::Error) -> Self {
        Self(error.code())
    }
}

impl From<LedgerError> for EffError {
    fn from(_: LedgerError) -> Self {
        Self::UNAVAILABLE
    }
}
