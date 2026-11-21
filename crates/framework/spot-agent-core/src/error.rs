//! The refusal vocabulary and the construction error, with stable `agent_core.*` codes.
//!
//! Every reason a command may not run is a [`Refusal`] with a stable machine-readable
//! code ([`Refusal::code`]); the codes are catalogued in
//! `spot-errors/catalogue/agent_core.toml` and are the only channel an operator needs.
//! Messages name the operation or the key id (a hash, never a secret) and never echo the
//! command's parameters.
//!
//! [`CoreError`] is the narrower vocabulary for building the trusted inputs (envelope,
//! trust store, executor), which is a host wiring mistake rather than a decision: it also
//! carries a stable code ([`CoreError::code`]).

use spot_signed_command::CommandError;
use thiserror::Error;

/// The command envelope is structurally wrong: not JSON, not the expected shape, an
/// invalid public key or signature, a non-canonical command, a foreign version.
pub const ENVELOPE_MALFORMED: &str = "agent_core.envelope_malformed";
/// The command names another agent as its audience.
pub const WRONG_AUDIENCE: &str = "agent_core.wrong_audience";
/// The command is not yet valid at the given instant (its issue instant is in the future
/// beyond the policy's clock skew).
pub const NOT_YET_VALID: &str = "agent_core.not_yet_valid";
/// The command is expired at the given instant.
pub const EXPIRED: &str = "agent_core.expired";
/// The command's validity window is longer than the policy allows.
pub const LIFETIME_EXCEEDED: &str = "agent_core.lifetime_exceeded";
/// A signature names an administrator key that is not in the trust store.
pub const UNKNOWN_SIGNER: &str = "agent_core.unknown_signer";
/// A signature names an administrator key revoked no later than the command's issue.
pub const REVOKED_SIGNER: &str = "agent_core.revoked_signer";
/// A signature does not verify under the key it names.
pub const INVALID_SIGNATURE: &str = "agent_core.invalid_signature";
/// Fewer distinct valid administrator signatures than the operation's risk requires.
pub const INSUFFICIENT_SIGNATURES: &str = "agent_core.insufficient_signatures";
/// The command's nonce was already accepted; it is a replay.
pub const REPLAY: &str = "agent_core.replay";
/// The nonce store refused the command (capacity or backend failure).
pub const NONCE_STORE: &str = "agent_core.nonce_store";
/// The command names an operation the catalogue does not register.
pub const UNKNOWN_OPERATION: &str = "agent_core.unknown_operation";
/// The command's parameters do not satisfy the operation's catalogue schema.
pub const INVALID_ARGUMENTS: &str = "agent_core.invalid_arguments";
/// A signature for a dangerous operation comes from a key that is not hardware-backed.
pub const SOFTWARE_KEY_REFUSED: &str = "agent_core.software_key_refused";
/// The requester signed a command the policy forbids the requester from approving.
pub const REQUESTER_CANNOT_APPROVE: &str = "agent_core.requester_cannot_approve";
/// A critical operation lacks the required configured delay or break-glass clearance.
pub const CRITICAL_NOT_CLEARED: &str = "agent_core.critical_not_cleared";
/// The result log could not record the decision, so the command was not run.
pub const LOG_UNAVAILABLE: &str = "agent_core.log_unavailable";
/// The executor has no handler registered for the operation.
pub const UNKNOWN_HANDLER: &str = "agent_core.unknown_handler";
/// A plan is not the deterministic plan of a validly signed command.
pub const PLAN_UNVERIFIED: &str = "agent_core.plan_unverified";
/// A plan's hash does not match its own fields.
pub const PLAN_HASH_MISMATCH: &str = "agent_core.plan_hash_mismatch";
/// A plan names a broker key the executor does not trust.
pub const UNTRUSTED_BROKER: &str = "agent_core.untrusted_broker";
/// A plan's broker signature does not verify.
pub const BROKER_SIGNATURE_INVALID: &str = "agent_core.broker_signature_invalid";
/// A public key offered for trust is not a usable Ed25519 public key.
pub const INVALID_KEY: &str = "agent_core.invalid_key";
/// The host supplied a malformed value while building the trusted inputs.
pub const MALFORMED: &str = "agent_core.malformed";
/// A hardware-key attestation did not verify.
pub const ATTESTATION: &str = "agent_core.attestation";

/// Why a command was refused. Each variant carries a stable code ([`Refusal::code`]).
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum Refusal {
    /// The envelope is structurally wrong (see [`ENVELOPE_MALFORMED`]).
    #[error("the command envelope is malformed")]
    EnvelopeMalformed,
    /// The command names another agent (see [`WRONG_AUDIENCE`]).
    #[error("the command is addressed to another agent")]
    WrongAudience,
    /// The command is not yet valid at the given instant (see [`NOT_YET_VALID`]).
    #[error("the command is not yet valid at the given time")]
    NotYetValid,
    /// The command is expired (see [`EXPIRED`]).
    #[error("the command is expired")]
    Expired,
    /// The validity window is too long (see [`LIFETIME_EXCEEDED`]).
    #[error("the command outlives the policy's maximum lifetime")]
    LifetimeExceeded,
    /// Unknown administrator key (see [`UNKNOWN_SIGNER`]).
    #[error("a signature names an administrator key that is not trusted")]
    UnknownSigner,
    /// Revoked administrator key (see [`REVOKED_SIGNER`]).
    #[error("a signature names an administrator key revoked before the command was issued")]
    RevokedSigner,
    /// Bad signature (see [`INVALID_SIGNATURE`]).
    #[error("a signature does not verify under the key it names")]
    InvalidSignature,
    /// Not enough distinct valid signatures for the operation's risk (see
    /// [`INSUFFICIENT_SIGNATURES`]).
    #[error("the command carries {have} distinct valid signatures but {need} are required")]
    InsufficientSignatures {
        /// How many distinct trusted keys verified.
        have: usize,
        /// How many the operation's risk level requires.
        need: usize,
    },
    /// A replay (see [`REPLAY`]).
    #[error("the command's nonce was already accepted")]
    Replay,
    /// The nonce store refused (see [`NONCE_STORE`]).
    #[error("the nonce store refused the command")]
    NonceStore,
    /// Unknown operation (see [`UNKNOWN_OPERATION`]).
    #[error("no operation with this id is registered in the catalogue")]
    UnknownOperation,
    /// Invalid arguments (see [`INVALID_ARGUMENTS`]).
    #[error("{op}: the command's parameters do not satisfy the operation's schema: {detail}")]
    InvalidArguments {
        /// The operation id.
        op: String,
        /// Which parameter problem was found, in words (never a value).
        detail: String,
    },
    /// A software key signed a dangerous operation (see [`SOFTWARE_KEY_REFUSED`]).
    #[error("signature from the administrator key {key_id} is not hardware-backed")]
    SoftwareKeyRefused {
        /// The offending key id (a hash, not a secret).
        key_id: String,
    },
    /// The requester approved (see [`REQUESTER_CANNOT_APPROVE`]).
    #[error("the requester {requester} may not approve this operation")]
    RequesterCannotApprove {
        /// The requester's key id.
        requester: String,
    },
    /// A critical operation was not cleared (see [`CRITICAL_NOT_CLEARED`]).
    #[error("{op}: a critical operation needs a configured delay or break-glass clearance")]
    CriticalNotCleared {
        /// The operation id.
        op: String,
    },
    /// The log refused to record (see [`LOG_UNAVAILABLE`]).
    #[error("the tamper-evident result log could not record the decision")]
    LogUnavailable,
    /// No handler for the operation (see [`UNKNOWN_HANDLER`]).
    #[error("{op}: no handler is registered for this operation")]
    UnknownHandler {
        /// The operation id.
        op: String,
    },
    /// The plan is not derived from a validly signed command (see [`PLAN_UNVERIFIED`]).
    #[error("the plan is not the deterministic plan of a validly signed command")]
    PlanUnverified,
    /// The plan hash does not match the plan's fields (see [`PLAN_HASH_MISMATCH`]).
    #[error("the plan hash does not match the plan's fields")]
    PlanHashMismatch,
    /// Untrusted broker (see [`UNTRUSTED_BROKER`]).
    #[error("the plan names a broker key the executor does not trust")]
    UntrustedBroker,
    /// Bad broker signature (see [`BROKER_SIGNATURE_INVALID`]).
    #[error("the broker's signature over the plan does not verify")]
    BrokerSignatureInvalid,
}

impl Refusal {
    /// The stable machine-readable code of this refusal, from the module docs. Never a
    /// parameter value and never a secret.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::EnvelopeMalformed => ENVELOPE_MALFORMED,
            Self::WrongAudience => WRONG_AUDIENCE,
            Self::NotYetValid => NOT_YET_VALID,
            Self::Expired => EXPIRED,
            Self::LifetimeExceeded => LIFETIME_EXCEEDED,
            Self::UnknownSigner => UNKNOWN_SIGNER,
            Self::RevokedSigner => REVOKED_SIGNER,
            Self::InvalidSignature => INVALID_SIGNATURE,
            Self::InsufficientSignatures { .. } => INSUFFICIENT_SIGNATURES,
            Self::Replay => REPLAY,
            Self::NonceStore => NONCE_STORE,
            Self::UnknownOperation => UNKNOWN_OPERATION,
            Self::InvalidArguments { .. } => INVALID_ARGUMENTS,
            Self::SoftwareKeyRefused { .. } => SOFTWARE_KEY_REFUSED,
            Self::RequesterCannotApprove { .. } => REQUESTER_CANNOT_APPROVE,
            Self::CriticalNotCleared { .. } => CRITICAL_NOT_CLEARED,
            Self::LogUnavailable => LOG_UNAVAILABLE,
            Self::UnknownHandler { .. } => UNKNOWN_HANDLER,
            Self::PlanUnverified => PLAN_UNVERIFIED,
            Self::PlanHashMismatch => PLAN_HASH_MISMATCH,
            Self::UntrustedBroker => UNTRUSTED_BROKER,
            Self::BrokerSignatureInvalid => BROKER_SIGNATURE_INVALID,
        }
    }
}

impl From<CommandError> for Refusal {
    /// Folds a signed-command failure into the decision vocabulary, keeping the same
    /// meaning. The constructor-side failures (an invalid key, a malformed command built
    /// locally) and the canonicity/version failures all read as a malformed envelope,
    /// since the command arrived over the wire.
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::WrongAudience => Self::WrongAudience,
            CommandError::NotYetValid => Self::NotYetValid,
            CommandError::Expired => Self::Expired,
            CommandError::LifetimeExceeded => Self::LifetimeExceeded,
            CommandError::UnknownKey => Self::UnknownSigner,
            CommandError::KeyRevoked => Self::RevokedSigner,
            CommandError::InvalidSignature => Self::InvalidSignature,
            CommandError::InsufficientSignatures { valid, required } => {
                Self::InsufficientSignatures {
                    have: valid,
                    need: required,
                }
            }
            CommandError::Replay => Self::Replay,
            CommandError::Store(_) => Self::NonceStore,
            CommandError::UnknownVersion { .. }
            | CommandError::NotCanonical
            | CommandError::InvalidKey
            | CommandError::Malformed(_) => Self::EnvelopeMalformed,
        }
    }
}

/// A host wiring mistake while building the trusted inputs: a key that is not a valid
/// Ed25519 point, a malformed value, or a hardware attestation that did not verify.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum CoreError {
    /// A public key is not a usable Ed25519 public key (see [`INVALID_KEY`]).
    #[error("the public key bytes are not a valid Ed25519 public key")]
    InvalidKey,
    /// A hardware-key attestation did not verify (see [`ATTESTATION`]).
    #[error("the hardware-key attestation did not verify")]
    Attestation,
    /// A value could not be built or encoded (see [`MALFORMED`]).
    #[error("the value is malformed: {0}")]
    Malformed(&'static str),
}

impl CoreError {
    /// The stable machine-readable code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidKey => INVALID_KEY,
            Self::Attestation => ATTESTATION,
            Self::Malformed(_) => MALFORMED,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn codes_are_dotted_lower_case() {
        // A representative sweep: every code the catalogue declares must be two dotted
        // lower-case segments.
        for code in [
            ENVELOPE_MALFORMED,
            INSUFFICIENT_SIGNATURES,
            CRITICAL_NOT_CLEARED,
            PLAN_UNVERIFIED,
        ] {
            assert!(code.starts_with("agent_core."));
            assert!(code.split('.').all(|s| {
                !s.is_empty()
                    && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && s.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            }));
        }
    }

    #[test]
    fn command_errors_fold_to_the_decision_vocabulary() {
        assert_eq!(
            Refusal::from(CommandError::WrongAudience),
            Refusal::WrongAudience
        );
        assert_eq!(
            Refusal::from(CommandError::InsufficientSignatures {
                valid: 1,
                required: 2
            }),
            Refusal::InsufficientSignatures { have: 1, need: 2 }
        );
        assert_eq!(Refusal::from(CommandError::Replay), Refusal::Replay);
        assert_eq!(
            Refusal::from(CommandError::Malformed("x")),
            Refusal::EnvelopeMalformed
        );
    }
}
