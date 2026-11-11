//! Typed errors with stable `link_external.*` codes (catalogued in
//! `spot-errors/catalogue/link_external.toml`).
//!
//! Nothing here echoes a secret, a signature, a header value, a payload value or free text from a
//! peer. Messages name fixed reasons only, so a failure can be logged, audited and returned to
//! the peer as it stands.

use thiserror::Error;

/// Stable codes of this crate.
pub mod codes {
    /// The peer definition is not valid.
    pub const INVALID_PEER: &str = "link_external.invalid_peer";
    /// A record topic is granted on an external link; only message topics may cross it.
    pub const RECORD_TOPIC: &str = "link_external.record_topic";
    /// The link is not an external-carrier link.
    pub const WRONG_CARRIER: &str = "link_external.wrong_carrier";
    /// The peer definition, the link and the signing identity do not name the same peer.
    pub const PEER_MISMATCH: &str = "link_external.peer_mismatch";
    /// The inbound request carries no signature.
    pub const MISSING_SIGNATURE: &str = "link_external.missing_signature";
    /// The inbound signature does not verify.
    pub const BAD_SIGNATURE: &str = "link_external.bad_signature";
    /// The inbound request is outside the timestamp window.
    pub const STALE_TIMESTAMP: &str = "link_external.stale_timestamp";
    /// The inbound request was sent to a path this peer does not use.
    pub const UNKNOWN_PATH: &str = "link_external.unknown_path";
    /// The inbound body is larger than the peer's cap.
    pub const BODY_TOO_LARGE: &str = "link_external.body_too_large";
    /// The peer's timestamp tolerance is wider than the maximum.
    pub const TOLERANCE_TOO_WIDE: &str = "link_external.tolerance_too_wide";
    /// The inbound body is not a message this link can carry.
    pub const INVALID_BODY: &str = "link_external.invalid_body";
    /// The peer format mapping failed.
    pub const MAPPING_FAILED: &str = "link_external.mapping_failed";
    /// The credential the peer refers to could not be resolved.
    pub const CREDENTIAL_UNAVAILABLE: &str = "link_external.credential_unavailable";
    /// The link is revoked, expired or not active, so nothing is accepted on it.
    pub const LINK_INACTIVE: &str = "link_external.link_inactive";
    /// The peer could not be reached.
    pub const PEER_UNREACHABLE: &str = "link_external.peer_unreachable";
    /// The peer did not answer within the deadline.
    pub const PEER_TIMEOUT: &str = "link_external.peer_timeout";
    /// The egress policy refused the destination.
    pub const EGRESS_DENIED: &str = "link_external.egress_denied";
    /// The peer's connector or endpoint is not usable as configured.
    pub const MISCONFIGURED: &str = "link_external.misconfigured";
    /// The peer answered with a status this carrier does not treat as an answer.
    pub const UNEXPECTED_STATUS: &str = "link_external.unexpected_status";

    /// Every code, so a scan sees each one.
    pub const ALL: [&str; 19] = [
        INVALID_PEER,
        RECORD_TOPIC,
        WRONG_CARRIER,
        PEER_MISMATCH,
        MISSING_SIGNATURE,
        BAD_SIGNATURE,
        STALE_TIMESTAMP,
        UNKNOWN_PATH,
        BODY_TOO_LARGE,
        TOLERANCE_TOO_WIDE,
        INVALID_BODY,
        MAPPING_FAILED,
        CREDENTIAL_UNAVAILABLE,
        LINK_INACTIVE,
        PEER_UNREACHABLE,
        PEER_TIMEOUT,
        EGRESS_DENIED,
        MISCONFIGURED,
        UNEXPECTED_STATUS,
    ];
}

/// Result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything this crate can refuse.
#[derive(Debug, Error)]
pub enum Error {
    /// The peer definition is not valid.
    #[error("the external peer is not valid: {reason}")]
    InvalidPeer {
        /// A fixed reason.
        reason: &'static str,
    },
    /// A record topic on an external link.
    #[error("a record topic cannot cross an external link: {topic}")]
    RecordTopic {
        /// The granted topic.
        topic: String,
    },
    /// The link is not an external-carrier link.
    #[error("the link does not use the external carrier")]
    WrongCarrier,
    /// The peer, the link or the signing identity name different peers.
    #[error("the peer, the link and the signing identity do not name the same peer")]
    PeerMismatch,
    /// No signature on an inbound request.
    #[error("the request carries no signature")]
    MissingSignature,
    /// The signature does not verify.
    #[error("the signature does not verify")]
    BadSignature,
    /// Outside the timestamp window.
    #[error("the request timestamp is outside the accepted window")]
    StaleTimestamp,
    /// Not this peer's path.
    #[error("no inbound endpoint is served at this path")]
    UnknownPath,
    /// The body is larger than this peer accepts.
    #[error("the request body is larger than this peer accepts")]
    BodyTooLarge,
    /// The timestamp tolerance is wider than the maximum this crate accepts.
    #[error("the timestamp tolerance is wider than 900 seconds")]
    ToleranceTooWide,
    /// The body cannot be carried.
    #[error("the body is not a message this link can carry: {reason}")]
    InvalidBody {
        /// A fixed reason.
        reason: &'static str,
    },
    /// The mapping failed.
    #[error("the peer format mapping failed")]
    MappingFailed,
    /// The credential could not be resolved.
    #[error("the credential the peer refers to is not available")]
    CredentialUnavailable,
    /// The link is not active.
    #[error("the link is not active")]
    LinkInactive,
    /// A failure of the exchange (outbox, inbox, stores, audit).
    #[error(transparent)]
    Exchange(#[from] spot_link_exchange::Error),
    /// A failure of the link model (grants, direction, expiry).
    #[error(transparent)]
    Link(#[from] spot_tenant_link::Error),
}

impl Error {
    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPeer { .. } => codes::INVALID_PEER,
            Self::RecordTopic { .. } => codes::RECORD_TOPIC,
            Self::WrongCarrier => codes::WRONG_CARRIER,
            Self::PeerMismatch => codes::PEER_MISMATCH,
            Self::MissingSignature => codes::MISSING_SIGNATURE,
            Self::BadSignature => codes::BAD_SIGNATURE,
            Self::StaleTimestamp => codes::STALE_TIMESTAMP,
            Self::UnknownPath => codes::UNKNOWN_PATH,
            Self::BodyTooLarge => codes::BODY_TOO_LARGE,
            Self::ToleranceTooWide => codes::TOLERANCE_TOO_WIDE,
            Self::InvalidBody { .. } => codes::INVALID_BODY,
            Self::MappingFailed => codes::MAPPING_FAILED,
            Self::CredentialUnavailable => codes::CREDENTIAL_UNAVAILABLE,
            Self::LinkInactive => codes::LINK_INACTIVE,
            Self::Exchange(error) => error.code(),
            Self::Link(error) => error.code(),
        }
    }

    /// The HTTP status an inbound request is answered with.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::MissingSignature | Self::BadSignature | Self::StaleTimestamp => 401,
            Self::UnknownPath => 404,
            Self::BodyTooLarge => 413,
            Self::InvalidBody { .. } | Self::MappingFailed => 400,
            Self::LinkInactive | Self::Link(_) | Self::PeerMismatch => 403,
            Self::CredentialUnavailable => 503,
            Self::Exchange(spot_link_exchange::Error::Conflict { .. }) => 503,
            Self::Exchange(spot_link_exchange::Error::NotFound) => 404,
            Self::Exchange(
                spot_link_exchange::Error::InvalidPayload { .. }
                | spot_link_exchange::Error::InvalidId { .. }
                | spot_link_exchange::Error::InvalidConversation { .. },
            ) => 400,
            _ => 500,
        }
    }
}
