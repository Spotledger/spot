//! Typed errors with stable `link_exchange.*` codes (catalogued in
//! `spot-errors/catalogue/link_exchange.toml`).
//!
//! Nothing here echoes a payload value, a key, a signature or free text from a peer. Messages
//! name fixed reasons only.

use thiserror::Error;

/// Stable error and rejection codes of this crate.
pub mod codes {
    /// An identifier is malformed.
    pub const INVALID_ID: &str = "link_exchange.invalid_id";
    /// A conversation name is malformed.
    pub const INVALID_CONVERSATION: &str = "link_exchange.invalid_conversation";
    /// A payload is not canonical JSON, too large, too deep or holds a float.
    pub const INVALID_PAYLOAD: &str = "link_exchange.invalid_payload";
    /// A retention setting is outside 1 to 365 days.
    pub const RETENTION_RANGE: &str = "link_exchange.retention_range";
    /// No such message.
    pub const NOT_FOUND: &str = "link_exchange.not_found";
    /// A store failed.
    pub const STORE_FAILED: &str = "link_exchange.store_failed";
    /// A store refused a change because the state moved underneath it.
    pub const CONFLICT: &str = "link_exchange.conflict";
    /// The audit trail could not be written, so the action was refused.
    pub const AUDIT_FAILED: &str = "link_exchange.audit_failed";
    /// The quarantine could not hold a refused input, so nothing was acknowledged.
    pub const QUARANTINE_FAILED: &str = "link_exchange.quarantine_failed";
    /// The quarantine cannot say how many inputs it holds, so a new refusal is not held.
    pub const QUARANTINE_COUNT_UNAVAILABLE: &str = "link_exchange.quarantine_count_unavailable";
    /// A message or receipt names another link than the one it was presented for.
    pub const LINK_MISMATCH: &str = "link_exchange.link_mismatch";
    /// The authenticated transport peer is not the link's peer.
    pub const IDENTITY_MISMATCH: &str = "link_exchange.identity_mismatch";
    /// A signature did not verify.
    pub const BAD_SIGNATURE: &str = "link_exchange.bad_signature";
    /// The link holds no pinned key for the peer.
    pub const UNKNOWN_KEY: &str = "link_exchange.unknown_key";
    /// The topic is not of the kind the operation needs.
    pub const WRONG_KIND: &str = "link_exchange.wrong_kind";
    /// The schema version is outside the grant's range.
    pub const SCHEMA_UNSUPPORTED: &str = "link_exchange.schema_unsupported";
    /// The receiver is busy; the sender keeps the message and sends it again.
    pub const BUSY: &str = "link_exchange.busy";
    /// Rejection code: the receiver supports none of the sent schema version.
    pub const VERSION_UNSUPPORTED: &str = "link_exchange.version_unsupported";
    /// Rejection code: the link does not grant the topic in this direction.
    pub const NOT_GRANTED: &str = "link_exchange.not_granted";
    /// Rejection code: the message arrived after its slot passed or after it expired.
    pub const OUT_OF_ORDER_EXPIRED: &str = "link_exchange.out_of_order_expired";
    /// Rejection code: the payload is invalid for the topic.
    pub const PAYLOAD_INVALID: &str = "link_exchange.payload_invalid";
    /// Rejection code: the link already holds as much parked work as it may.
    pub const CAPACITY_EXCEEDED: &str = "link_exchange.capacity_exceeded";
}

/// Result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything this crate can refuse.
#[derive(Debug, Error)]
pub enum Error {
    /// An identifier is malformed.
    #[error("the identifier is not valid: {reason}")]
    InvalidId {
        /// A fixed reason.
        reason: &'static str,
    },
    /// A conversation name is malformed.
    #[error("the conversation name is not valid: {reason}")]
    InvalidConversation {
        /// A fixed reason.
        reason: &'static str,
    },
    /// A payload is unusable.
    #[error("the payload is not valid: {reason}")]
    InvalidPayload {
        /// A fixed reason.
        reason: &'static str,
    },
    /// A retention setting is out of range.
    #[error("the message retention must be 1 to 365 days")]
    RetentionRange,
    /// No such message.
    #[error("no such message")]
    NotFound,
    /// A store failed.
    #[error("the exchange store failed: {reason}")]
    StoreFailed {
        /// A fixed reason.
        reason: &'static str,
    },
    /// A store refused a change because the state moved underneath it.
    #[error("the exchange state changed underneath the operation: {reason}")]
    Conflict {
        /// A fixed reason.
        reason: &'static str,
    },
    /// The audit trail refused the record.
    #[error("the audit trail could not record {action}")]
    AuditFailed {
        /// The action that was refused.
        action: &'static str,
    },
    /// The quarantine could not hold a refused input.
    #[error("the quarantine could not hold a refused input")]
    QuarantineFailed,
    /// A quarantine that cannot say how many inputs it holds for a link.
    ///
    /// How much a partner may make this tenant accumulate is bounded by the quarantine's own count
    /// ([`crate::inbox::InputQuarantine::held_count`]). An implementer that cannot answer says so
    /// with this rather than answering zero, and a caller that must decide fails closed on it: a
    /// refused input is dropped instead of held, while the receipt of valid messages is not
    /// affected. Answering zero instead would lift the bound after a restart, over a quarantine
    /// that is still full.
    #[error("the input quarantine cannot say how many inputs it holds for the link")]
    QuarantineCountUnavailable,
    /// A message or receipt names another link.
    #[error("the item belongs to another link")]
    LinkMismatch,
    /// The transport peer is not the link's peer.
    #[error("the authenticated peer is not the link's peer")]
    IdentityMismatch,
    /// A signature did not verify.
    #[error("the signature did not verify")]
    BadSignature,
    /// The link holds no pinned key for the peer.
    #[error("the link holds no pinned key for the peer")]
    UnknownKey,
    /// The topic is of the other kind.
    #[error("the topic is a {expected} topic")]
    WrongKind {
        /// `record` or `message`.
        expected: &'static str,
    },
    /// The schema version is outside the grant's range.
    #[error("the schema version is outside the granted range")]
    SchemaUnsupported,
    /// The receiver is busy.
    #[error("the receiver is busy; send again later")]
    Busy,
    /// The tenant link refused.
    #[error(transparent)]
    Link(#[from] spot_tenant_link::Error),
}

impl Error {
    /// The stable dotted code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => codes::INVALID_ID,
            Self::InvalidConversation { .. } => codes::INVALID_CONVERSATION,
            Self::InvalidPayload { .. } => codes::INVALID_PAYLOAD,
            Self::RetentionRange => codes::RETENTION_RANGE,
            Self::NotFound => codes::NOT_FOUND,
            Self::StoreFailed { .. } => codes::STORE_FAILED,
            Self::Conflict { .. } => codes::CONFLICT,
            Self::AuditFailed { .. } => codes::AUDIT_FAILED,
            Self::QuarantineFailed => codes::QUARANTINE_FAILED,
            Self::QuarantineCountUnavailable => codes::QUARANTINE_COUNT_UNAVAILABLE,
            Self::LinkMismatch => codes::LINK_MISMATCH,
            Self::IdentityMismatch => codes::IDENTITY_MISMATCH,
            Self::BadSignature => codes::BAD_SIGNATURE,
            Self::UnknownKey => codes::UNKNOWN_KEY,
            Self::WrongKind { .. } => codes::WRONG_KIND,
            Self::SchemaUnsupported => codes::SCHEMA_UNSUPPORTED,
            Self::Busy => codes::BUSY,
            Self::Link(error) => error.code(),
        }
    }
}
