//! The one error type for enrolment, with stable machine-readable codes.
//!
//! Messages are plain language, never echo untrusted input, and never contain token
//! text, key bytes or seeds. Callers match on [`EnrolError::code`], never on the
//! message. Where the protocol deliberately hides a outcome's reason from the peer
//! (refused tokens), the caller-facing variant is one and the same; the precise reason
//! lives in the audit log only.

use std::fmt;

/// Something failed while storing or enrolling an identity.
///
/// This is the one error type of this crate. The [`EnrolError::code`] values are this
/// crate's API contract and never change in a breaking way.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnrolError {
    /// The presented token text has the wrong shape: length, alphabet or checksum
    /// group. Checked before any lookup, so a typo never reaches the token store.
    #[error("enrolment token does not have the expected shape")]
    #[cfg_attr(docsrs, doc(cfg(all)))]
    TokenMalformed,

    /// The token was refused — it was unknown, already used, or expired. One outcome
    /// for all three: no caller, friendly or hostile, learns which.
    #[error("enrolment token was refused")]
    TokenRefused,

    /// A `FileKeyStore` file could not be read or written; the reason is this variant,
    /// never the underlying OS text (which could carry a path).
    #[error("the identity key file could not be used")]
    KeyFileIo,

    /// A key-store file did not hold the fields this crate wrote, or the wrong types.
    #[error("the identity key file is malformed")]
    KeyFileMalformed,

    /// A Unix key file's permissions allow group or other access; refused.
    #[error("the identity key file is readable by more than its owner")]
    KeyFilePermissions,

    /// The server's public key or a presented signature has the wrong byte length or
    /// does not decode: the input is never echoed.
    #[error("a key or signature does not have the expected shape")]
    CryptoMalformed,

    /// A challenge signature did not verify, or a proof did not verify. One variant
    /// for both: neither side ever learns *why* a signature failed.
    #[error("a signature did not verify")]
    SignatureInvalid,

    /// The agent aborted: the server key in the challenge does not match the pinned
    /// expectation from the install command. Happens before any proof leaves the agent.
    #[error("the server public key does not match the pinned identity")]
    ServerKeyMismatch,

    /// A message arrived that the state machine cannot accept at this point: a proof
    /// before a challenge, a second request during an exchange, and so on.
    #[error("an enrolment message is out of order for this state")]
    MessageOutOfOrder,

    /// A field of a message is empty or outside its accepted length range.
    #[error("an enrolment message field is out of the accepted range")]
    FieldInvalid,

    /// The persisted [`crate::identity::AgentIdentity`] file could not be read or
    /// written.
    #[error("the agent identity file could not be used")]
    IdentityFileIo,

    /// The identity file did not parse, had extra or missing fields, or fields of the
    /// wrong shape (a wrong-length key, an unparsable timestamp).
    #[error("the agent identity file is not exactly one valid identity")]
    IdentityFileMalformed,
}

impl EnrolError {
    /// The machine-readable code of this error: `agent-enroll.<name>`, stable.
    #[must_use]
    pub fn code(&self) -> String {
        let name = match self {
            Self::TokenMalformed => "token_malformed",
            Self::TokenRefused => "token_refused",
            Self::KeyFileIo => "key_file_io",
            Self::KeyFileMalformed => "key_file_malformed",
            Self::KeyFilePermissions => "key_file_permissions",
            Self::CryptoMalformed => "crypto_malformed",
            Self::SignatureInvalid => "signature_invalid",
            Self::ServerKeyMismatch => "server_key_mismatch",
            Self::MessageOutOfOrder => "message_out_of_order",
            Self::FieldInvalid => "field_invalid",
            Self::IdentityFileIo => "identity_file_io",
            Self::IdentityFileMalformed => "identity_file_malformed",
        };
        format!("agent-enroll.{name}")
    }
}

impl fmt::Debug for EnrolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EnrolError({})", self.code())
    }
}

/// A refusal of an enrolment attempt, with the reason visible to the server's audit
/// log only.
///
/// This is the return of a refused [`crate::service::EnrolmentService::request`]: to
/// the agent it carries no reason at all (`Display` says exactly one sentence); the
/// caller on the server side chooses how much of [`RejectReason`] reaches its audit
/// trail and nothing leaves the service boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RejectReason {
    /// The token text was not known to the store.
    UnknownToken,
    /// The token was already consumed by an earlier request.
    AlreadyUsed,
    /// The token had expired at the instant of the request.
    Expired,
}

impl RejectReason {
    /// The stable audit code: `agent-enroll.reject.<name>`.
    #[must_use]
    pub fn code(&self) -> String {
        let name = match self {
            Self::UnknownToken => "unknown_token",
            Self::AlreadyUsed => "already_used",
            Self::Expired => "expired",
        };
        format!("agent-enroll.reject.{name}")
    }
}

impl fmt::Display for RejectReason {
    /// Deliberately uninformative: outside the audit log, every refusal reads the same.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("enrolment refused")
    }
}
