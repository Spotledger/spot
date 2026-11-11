//! The errors of the signed command envelope with their stable `cmd.*` codes
//! (`WP-ADM-01.T2`).
//!
//! Every variant carries a stable machine-readable code ([`CommandError::code`]); the
//! codes the verifier produces, in the order its checks run:
//!
//! | Variant | Code |
//! | --- | --- |
//! | [`CommandError::UnknownVersion`] | `cmd.unknown_version` |
//! | [`CommandError::NotCanonical`] | `cmd.not_canonical` |
//! | [`CommandError::WrongAudience`] | `cmd.wrong_audience` |
//! | [`CommandError::NotYetValid`] | `cmd.not_yet_valid` |
//! | [`CommandError::Expired`] | `cmd.expired` |
//! | [`CommandError::LifetimeExceeded`] | `cmd.lifetime_exceeded` |
//! | [`CommandError::UnknownKey`] | `cmd.unknown_key` |
//! | [`CommandError::KeyRevoked`] | `cmd.key_revoked` |
//! | [`CommandError::InvalidSignature`] | `cmd.invalid_signature` |
//! | [`CommandError::InsufficientSignatures`] | `cmd.insufficient_signatures` |
//! | [`CommandError::Replay`] | `cmd.replay` |
//! | [`CommandError::Store`] | `cmd.nonce_store` |
//! | [`CommandError::InvalidKey`] | `cmd.invalid_key` |
//! | [`CommandError::Malformed`] | `cmd.malformed` |
//!
//! Error messages never echo the command's `params` or any other untrusted payload, so
//! the codes are also the only channel an operator needs.

use thiserror::Error;

/// Failure of a signed-command operation: construction, signing, verification, or the
/// nonce store.
#[derive(Debug, Error)]
pub enum CommandError {
    /// The command names a format version this crate does not implement.
    #[error("the command's format version is not supported")]
    UnknownVersion {
        /// The unsupported version number the command claimed.
        version: u8,
    },
    /// The command contains a JSON float, which has no canonical bytes, so nothing can
    /// be signed or verified. Integers and strings only, per the canonical rules.
    #[error("the command contains a JSON number without a canonical form")]
    NotCanonical,
    /// The command is addressed to a different agent; the verifier's agent id never
    /// appears in the message.
    #[error("the command is addressed to another agent")]
    WrongAudience,
    /// `now` is before the command's validity window start minus the skew tolerance.
    #[error("the command is not yet valid at the given time")]
    NotYetValid,
    /// `now` is at or past the command's expiry instant.
    #[error("the command is expired")]
    Expired,
    /// The command's window is longer than the verifier policy allows.
    #[error("the command outlives the policy's maximum lifetime")]
    LifetimeExceeded,
    /// A signature names a key id the trusted set does not have.
    #[error("a signature names a key that is not trusted")]
    UnknownKey,
    /// A signature names a trusted key that was revoked no later than the command's
    /// issue instant.
    #[error("a signature names a key that was revoked before the command was issued")]
    KeyRevoked,
    /// A signature does not verify under the trusted public key it names; the signed
    /// bytes were altered or the signature was made otherwise.
    #[error("a signature does not verify under the trusted key it names")]
    InvalidSignature,
    /// Fewer distinct trusted valid signatures than the policy demands.
    #[error("the command does not carry enough distinct trusted signatures")]
    InsufficientSignatures {
        /// How many distinct trusted keys verified.
        valid: usize,
        /// How many the policy demands.
        required: usize,
    },
    /// The nonce was already accepted; the command is a replay.
    #[error("the command's nonce was already accepted")]
    Replay,
    /// The nonce store refused the command (capacity, or a backend failure).
    #[error("the nonce store refused the command: {0}")]
    Store(#[from] StoreError),
    /// The public key bytes offered for trust are not a usable Ed25519 public key.
    #[error("the public key bytes are not a valid Ed25519 public key")]
    InvalidKey,
    /// The signed command bytes are structurally broken (missing or ill-typed fields,
    /// an unknown field, bad key ids or nonces).
    #[error("the signed command is malformed: {0}")]
    Malformed(&'static str),
}

impl CommandError {
    /// The stable machine-readable code of this error, from the table in the module
    /// docs. Never `params`, never a key id, never any untrusted text.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownVersion { .. } => concat!("cmd", ".", "unknown_version"),
            Self::NotCanonical => concat!("cmd", ".", "not_canonical"),
            Self::WrongAudience => concat!("cmd", ".", "wrong_audience"),
            Self::NotYetValid => concat!("cmd", ".", "not_yet_valid"),
            Self::Expired => concat!("cmd", ".", "expired"),
            Self::LifetimeExceeded => concat!("cmd", ".", "lifetime_exceeded"),
            Self::UnknownKey => concat!("cmd", ".", "unknown_key"),
            Self::KeyRevoked => concat!("cmd", ".", "key_revoked"),
            Self::InvalidSignature => concat!("cmd", ".", "invalid_signature"),
            Self::InsufficientSignatures { .. } => concat!("cmd", ".", "insufficient_signatures"),
            Self::Replay => concat!("cmd", ".", "replay"),
            Self::Store(_) => concat!("cmd", ".", "nonce_store"),
            Self::InvalidKey => concat!("cmd", ".", "invalid_key"),
            Self::Malformed(_) => concat!("cmd", ".", "malformed"),
        }
    }
}

/// Failure of a [`crate::nonce::NonceStore`] implementation.
///
/// The store is the last check before acceptance, so a failure here refuses the
/// command; it never blocks or evicts a live nonce.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum StoreError {
    /// The store reached its hard cap and refuses new commands instead of evicting
    /// live entries (evicting would reopen the replay window).
    #[error("the nonce store is full and refuses new entries")]
    Full,
    /// The backend failed for a reason outside this crate's contract (I/O, lock
    /// poisoning). The command is refused.
    #[error("the nonce store backend failed: {0}")]
    Backend(String),
    /// The store could not answer (no connection, a failed statement, a timeout). The
    /// answer is unknown, so the command is refused; carries no driver text.
    #[error("the nonce store is unavailable")]
    Unavailable,
}
