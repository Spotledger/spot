//! Typed errors with stable `journal_protect.*` codes.
//!
//! A message names a thing (a code, a reason phrase, a record id) and never a value: not the
//! plaintext of a personal field, not a ciphertext, not a key.

use spot_journal_pii::PiiError;
use spot_journal_relay::RelayError;

/// Every error the transforms return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtectError {
    /// The source of personal-field descriptions failed, so nothing can be said about whether the
    /// entity has personal fields. The image is not written.
    #[error("the personal-field description source failed: {reason}")]
    SpecUnavailable {
        /// What the source said (a fixed phrase, never a value).
        reason: String,
    },
    /// An entry or an image cannot be transformed: it is not an envelope, a state entry has no
    /// version, or an image is not an object.
    #[error("the entry cannot be transformed: {0}")]
    EntryInvalid(&'static str),
    /// The image names a record id or a version other than the one the envelope or the record
    /// names, so the envelope would be bound to the wrong record.
    #[error("the image names another record identity than its entry: {0}")]
    IdentityMismatch(&'static str),
    /// A personal field could not be sealed (no key for its subject, the key store failed, no
    /// nonce, a malformed image). The image is not written.
    #[error("a personal field could not be protected: {0}")]
    Protect(PiiError),
    /// A personal field could not be revealed (the key store is down, an envelope is tampered with
    /// or moved). The rebuild stops; nothing is replaced by a placeholder on this path.
    #[error("a personal field could not be revealed: {0}")]
    Reveal(PiiError),
}

impl ProtectError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SpecUnavailable { .. } => "journal_protect.spec_unavailable",
            Self::EntryInvalid(_) => "journal_protect.entry_invalid",
            Self::IdentityMismatch(_) => "journal_protect.identity_mismatch",
            Self::Protect(_) => "journal_protect.protect_failed",
            Self::Reveal(_) => "journal_protect.reveal_failed",
        }
    }

    /// The `journal_pii.*` code underneath, when the failure came from the mechanism.
    #[must_use]
    pub fn pii_code(&self) -> Option<&'static str> {
        match self {
            Self::Protect(error) | Self::Reveal(error) => Some(error.code()),
            _ => None,
        }
    }
}

/// Why a protected append did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppendError {
    /// Protecting the batch failed; the relay was not called and nothing was written.
    #[error("the batch was not protected, so nothing was written: {0}")]
    Protect(ProtectError),
    /// The relay refused the protected batch.
    #[error("the relay refused the protected batch: {0}")]
    Relay(RelayError),
}

impl AppendError {
    /// The stable catalogue code of whichever side failed.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Protect(error) => error.code(),
            Self::Relay(error) => error.code(),
        }
    }
}
