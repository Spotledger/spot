//! Typed errors with stable `audit.*` codes.

/// Every error this crate returns. Messages never carry a payload value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditError {
    /// The payload is larger than the limit once canonically encoded.
    #[error("the payload is {size} bytes encoded; the limit is {limit}")]
    PayloadTooLarge {
        /// The encoded size.
        size: usize,
        /// The limit.
        limit: usize,
    },
    /// A floating-point number was offered as a payload value.
    #[error("a floating-point number cannot be an audit payload value")]
    FloatRefused,
    /// The record is malformed.
    #[error("the audit record is malformed: {0}")]
    RecordInvalid(&'static str),
    /// Other writers kept moving the head; the append gave up.
    #[error("the chain head kept moving; the append gave up")]
    HeadConflict,
    /// The store failed, or refused an entry that does not extend its head.
    #[error("the audit store failed: {0}")]
    Store(String),
    /// The checkpoint signer could not sign.
    #[error("the checkpoint signer failed: {0}")]
    Signer(String),
    /// A public key is not a valid Ed25519 key.
    #[error("a public key is not a valid Ed25519 key")]
    KeyInvalid,
}

impl AuditError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PayloadTooLarge { .. } => "audit.payload_too_large",
            Self::FloatRefused => "audit.float_refused",
            Self::RecordInvalid(_) => "audit.record_invalid",
            Self::HeadConflict => "audit.head_conflict",
            Self::Store(_) => "audit.store",
            Self::Signer(_) => "audit.signer",
            Self::KeyInvalid => "audit.key_invalid",
        }
    }
}
