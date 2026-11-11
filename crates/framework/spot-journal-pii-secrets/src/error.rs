//! Typed errors with stable `journal_pii_secrets.*` codes.
//!
//! A message names no key, no subject and no wrapped byte. The provider's own error text is
//! never copied in: only its stable code is, so a provider that put detail in a message cannot
//! leak it through this crate. The text of [`SecretsPiiError::Store`], [`SecretsPiiError::Audit`]
//! and [`SecretsPiiError::Rng`] comes from the caller's port and must describe the failure only.

use spot_journal_pii::KeyStoreError;

/// Every error this crate returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretsPiiError {
    /// The tenant journal key was asked to be destroyed.
    #[error("the tenant journal key cannot be destroyed through the subject-key adapter")]
    RefuseJournalKey,
    /// The adapter's configuration is unusable.
    #[error("the adapter configuration is invalid: {0}")]
    ConfigInvalid(&'static str),
    /// The key provider failed to wrap or unwrap a key.
    #[error("the key provider failed ({0})")]
    Provider(&'static str),
    /// The wrapped-key store failed.
    #[error("the wrapped-key store failed: {0}")]
    Store(String),
    /// The audit port refused the event.
    #[error("the audit port failed: {0}")]
    Audit(String),
    /// The random source failed.
    #[error("the random source failed: {0}")]
    Rng(String),
    /// A stored entry is not one this adapter can use.
    #[error("a stored key entry is unusable: {0}")]
    EntryInvalid(&'static str),
}

impl SecretsPiiError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RefuseJournalKey => "journal_pii_secrets.refuse_journal_key",
            Self::ConfigInvalid(_) => "journal_pii_secrets.config_invalid",
            Self::Provider(_) => "journal_pii_secrets.provider_failed",
            Self::Store(_) => "journal_pii_secrets.store_failed",
            Self::Audit(_) => "journal_pii_secrets.audit_failed",
            Self::Rng(_) => "journal_pii_secrets.rng_failed",
            Self::EntryInvalid(_) => "journal_pii_secrets.entry_invalid",
        }
    }
}

impl From<SecretsPiiError> for KeyStoreError {
    /// Every failure reaches `spot-journal-pii` as `Unavailable`, never as "no key": that is
    /// what keeps an outage from reading as an erasure.
    fn from(error: SecretsPiiError) -> Self {
        KeyStoreError::Unavailable(format!("{}: {error}", error.code()))
    }
}
