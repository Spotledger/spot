//! Typed errors with stable `tenant_link.*` codes (catalogued in
//! `spot-errors/catalogue/tenant_link.toml`).
//!
//! Nothing here echoes a private key, a payload value, free text from a peer or a credential.
//! Public keys are named by fingerprint and field names by name; a refusal detail is a fixed
//! word such as `direction` or `schema_range`.

/// Stable error codes of this crate.
pub mod codes {
    /// An identifier is malformed.
    pub const INVALID_ID: &str = "tenant_link.invalid_id";
    /// A party is malformed.
    pub const INVALID_PARTY: &str = "tenant_link.invalid_party";
    /// A topic is malformed.
    pub const INVALID_TOPIC: &str = "tenant_link.invalid_topic";
    /// A grant is malformed.
    pub const INVALID_GRANT: &str = "tenant_link.invalid_grant";
    /// A schema range is malformed.
    pub const INVALID_SCHEMA_RANGE: &str = "tenant_link.invalid_schema_range";
    /// A field name is malformed.
    pub const INVALID_FIELD: &str = "tenant_link.invalid_field";
    /// A protected field was covered by a namespace rule instead of being listed by name.
    pub const FIELD_CLASSIFICATION: &str = "tenant_link.field_classification";
    /// An offer is malformed.
    pub const INVALID_OFFER: &str = "tenant_link.invalid_offer";
    /// An expiry is malformed or in the past.
    pub const INVALID_EXPIRY: &str = "tenant_link.invalid_expiry";
    /// An external link named a record topic (ADR-058 rule 12).
    pub const EXTERNAL_RECORD_TOPIC: &str = "tenant_link.external_record_topic";
    /// No such link.
    pub const NOT_FOUND: &str = "tenant_link.not_found";
    /// The link changed since it was read.
    pub const VERSION_CONFLICT: &str = "tenant_link.version_conflict";
    /// A link with this id or this party pair already exists.
    pub const DUPLICATE_LINK: &str = "tenant_link.duplicate_link";
    /// The link state machine does not allow the transition.
    pub const INVALID_TRANSITION: &str = "tenant_link.invalid_transition";
    /// The link is not active.
    pub const NOT_ACTIVE: &str = "tenant_link.not_active";
    /// The link is revoked.
    pub const REVOKED: &str = "tenant_link.revoked";
    /// The link is past its expiry.
    pub const LINK_EXPIRED: &str = "tenant_link.link_expired";
    /// The grant for the topic is past its expiry.
    pub const GRANT_EXPIRED: &str = "tenant_link.grant_expired";
    /// The link holds no grant for the topic.
    pub const TOPIC_NOT_GRANTED: &str = "tenant_link.topic_not_granted";
    /// The grant does not cover the requested direction.
    pub const WRONG_DIRECTION: &str = "tenant_link.wrong_direction";
    /// The two offers disagree about a topic.
    pub const OFFER_CONFLICT: &str = "tenant_link.offer_conflict";
    /// A grant is wider than one of the offers.
    pub const GRANT_WIDER: &str = "tenant_link.grant_wider";
    /// The peer's region is refused by a residency clause.
    pub const REGION_REFUSED: &str = "tenant_link.region_refused";
    /// The peer is not in the region the grant requires.
    pub const PEER_REGION_MISMATCH: &str = "tenant_link.peer_region_mismatch";
    /// A handshake signature did not verify.
    pub const BAD_SIGNATURE: &str = "tenant_link.bad_signature";
    /// A handshake message came from the wrong key.
    pub const WRONG_SIGNER: &str = "tenant_link.wrong_signer";
    /// A peer identity key is not the one this side trusts.
    pub const KEY_MISMATCH: &str = "tenant_link.key_mismatch";
    /// A party has no pinned identity key and its key was not confirmed on first use.
    pub const KEY_UNPINNED: &str = "tenant_link.key_unpinned";
    /// A handshake message was replayed.
    pub const REPLAYED: &str = "tenant_link.replayed";
    /// A handshake message arrived out of order.
    pub const OUT_OF_ORDER: &str = "tenant_link.out_of_order";
    /// A handshake message does not continue the transcript.
    pub const TRANSCRIPT_MISMATCH: &str = "tenant_link.transcript_mismatch";
    /// A handshake message is not the step the exchange expects.
    pub const WRONG_STEP: &str = "tenant_link.wrong_step";
    /// A handshake message names another link.
    pub const WRONG_LINK: &str = "tenant_link.wrong_link";
    /// The link does not have both consents.
    pub const CONSENT_REQUIRED: &str = "tenant_link.consent_required";
    /// The same actor cannot consent for both parties.
    pub const CONSENT_DUPLICATE: &str = "tenant_link.consent_duplicate";
    /// A consent is malformed.
    pub const CONSENT_INVALID: &str = "tenant_link.consent_invalid";
    /// A consent has no fresh strong sign-in.
    pub const STEP_UP_REQUIRED: &str = "tenant_link.step_up_required";
    /// A consent is older than the policy allows when it is used.
    pub const CONSENT_STALE: &str = "tenant_link.consent_stale";
    /// The audit trail could not record the event, so the operation did not proceed.
    pub const AUDIT_FAILED: &str = "tenant_link.audit_failed";
    /// The link store failed.
    pub const STORE_FAILED: &str = "tenant_link.store_failed";
}

/// Anything the link crate refuses to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// An identifier (link id, peer id or nonce) is malformed.
    #[error("an identifier is not valid: {reason}")]
    InvalidId {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A party is malformed.
    #[error("the party is not valid: {reason}")]
    InvalidParty {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A topic is malformed.
    #[error("the topic is not valid: {reason}")]
    InvalidTopic {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A grant is malformed.
    #[error("the grant is not valid: {reason}")]
    InvalidGrant {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A schema range is malformed.
    #[error("the schema range is not valid: {reason}")]
    InvalidSchemaRange {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A field name is malformed.
    #[error("the field name is not valid: {reason}")]
    InvalidField {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A protected field is covered by a namespace rule, not listed by name.
    #[error("the field {field} is {class} and must be listed explicitly")]
    FieldClassification {
        /// The field.
        field: String,
        /// `pii` or `sensitive`.
        class: &'static str,
    },
    /// An offer is malformed.
    #[error("the offer is not valid: {reason}")]
    InvalidOffer {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// An expiry is malformed or already past.
    #[error("the expiry is not valid: {reason}")]
    InvalidExpiry {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// An external link named a record topic.
    #[error("an external link cannot carry the record topic {topic}")]
    ExternalRecordTopic {
        /// The topic.
        topic: String,
    },
    /// No such link.
    #[error("link {link} was not found")]
    NotFound {
        /// The link id.
        link: String,
    },
    /// The link changed since it was read.
    #[error("link {link} changed since it was read (expected version {expected}, found {found})")]
    VersionConflict {
        /// The link id.
        link: String,
        /// The version the caller read.
        expected: u64,
        /// The version stored.
        found: u64,
    },
    /// A link with this id or this party pair already exists.
    #[error("a link with this id or party pair already exists: {link}")]
    DuplicateLink {
        /// The link id.
        link: String,
    },
    /// The state machine does not allow the transition.
    #[error("link {link} cannot move from {from} to {to}")]
    InvalidTransition {
        /// The link id.
        link: String,
        /// Current state.
        from: crate::model::LinkState,
        /// Requested state.
        to: crate::model::LinkState,
    },
    /// The link is not active.
    #[error("link {link} is {state} and cannot be used")]
    NotActive {
        /// The link id.
        link: String,
        /// Current state.
        state: crate::model::LinkState,
    },
    /// The link is revoked.
    #[error("link {link} was revoked")]
    Revoked {
        /// The link id.
        link: String,
    },
    /// The link is past its expiry.
    #[error("link {link} expired")]
    LinkExpired {
        /// The link id.
        link: String,
    },
    /// The grant for the topic is past its expiry.
    #[error("the grant for topic {topic} on link {link} expired")]
    GrantExpired {
        /// The link id.
        link: String,
        /// The topic.
        topic: String,
    },
    /// The link holds no grant for the topic.
    #[error("link {link} has no grant for topic {topic}")]
    TopicNotGranted {
        /// The link id.
        link: String,
        /// The topic.
        topic: String,
    },
    /// The grant does not cover the requested direction.
    #[error("the grant for topic {topic} on link {link} does not allow that direction")]
    WrongDirection {
        /// The link id.
        link: String,
        /// The topic.
        topic: String,
    },
    /// The two offers disagree about a topic.
    #[error("the offers disagree on topic {topic}: {reason}")]
    OfferConflict {
        /// The topic.
        topic: String,
        /// Which dimension disagrees.
        reason: &'static str,
    },
    /// A grant is wider than one of the offers.
    #[error("the grant for topic {topic} is wider than the offers: {dimension}")]
    GrantWider {
        /// The topic.
        topic: String,
        /// Which dimension is wider.
        dimension: &'static str,
    },
    /// The peer's region is refused by a residency clause.
    #[error("the peer region {region} is refused by {clause}")]
    RegionRefused {
        /// The clause, as `spot-data-policy` names it.
        clause: String,
        /// The region.
        region: String,
    },
    /// The peer is not in the region the grant requires.
    #[error("the peer is in region {found} but the grant requires {required}")]
    PeerRegionMismatch {
        /// The region the grant requires.
        required: String,
        /// The region the oracle reported.
        found: String,
    },
    /// A handshake message signature did not verify.
    #[error("a handshake message signature did not verify")]
    BadSignature,
    /// A handshake message came from the wrong key.
    #[error("the handshake message was signed by {found}, not by {expected}")]
    WrongSigner {
        /// The expected key fingerprint.
        expected: String,
        /// The presented key fingerprint.
        found: String,
    },
    /// A peer identity key differs from the one this side has pinned or confirmed.
    #[error(
        "the identity key of {party} is not the trusted one: expected {expected}, found {found}"
    )]
    KeyMismatch {
        /// The party whose key was judged.
        party: String,
        /// The pinned or administrator-confirmed fingerprint.
        expected: String,
        /// The fingerprint of the key in the message.
        found: String,
    },
    /// A party has no pinned key and the caller did not confirm this key on first use.
    #[error("party {party} has no pinned identity key and this key was not confirmed on first use")]
    KeyUnpinned {
        /// The party whose key was judged.
        party: String,
    },
    /// A handshake message was replayed.
    #[error("handshake message {revision} was already received")]
    Replayed {
        /// The revision that arrived twice.
        revision: u64,
    },
    /// A handshake message arrived out of order.
    #[error("handshake message {found} arrived where {expected} was expected")]
    OutOfOrder {
        /// The revision expected.
        expected: u64,
        /// The revision presented.
        found: u64,
    },
    /// A handshake message does not continue the transcript.
    #[error("the handshake message does not continue the transcript")]
    TranscriptMismatch,
    /// A handshake message is not the step the exchange expects.
    #[error("handshake step {found} arrived where {expected} was expected")]
    WrongStep {
        /// The step expected.
        expected: &'static str,
        /// The step presented.
        found: &'static str,
    },
    /// A handshake message names another link.
    #[error("the message names link {found}, not {expected}")]
    WrongLink {
        /// The link expected.
        expected: String,
        /// The link named.
        found: String,
    },
    /// The link does not have both consents.
    #[error("link {link} needs two distinct consents before it can be activated")]
    ConsentRequired {
        /// The link id.
        link: String,
    },
    /// The same actor cannot consent for both parties.
    #[error("the consent of {actor} cannot stand for both parties")]
    ConsentDuplicate {
        /// The actor.
        actor: String,
    },
    /// A consent is malformed.
    #[error("the consent is not valid: {reason}")]
    ConsentInvalid {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A consent has no fresh strong sign-in.
    #[error("the consent of {actor} has no recent strong sign-in")]
    StepUpRequired {
        /// The actor.
        actor: String,
    },
    /// A consent is older than the policy allows when it is used.
    #[error("the consent of {actor} is older than the allowed {max_age_ms} milliseconds")]
    ConsentStale {
        /// The actor.
        actor: String,
        /// The maximum age the caller's policy allows.
        max_age_ms: u64,
    },
    /// The audit trail could not record the event.
    #[error("the audit trail could not record {action}")]
    AuditFailed {
        /// The action that was refused.
        action: &'static str,
    },
    /// The link store failed.
    #[error("the link store failed: {reason}")]
    StoreFailed {
        /// A fixed reason.
        reason: &'static str,
    },
}

impl Error {
    /// The stable dotted code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => codes::INVALID_ID,
            Self::InvalidParty { .. } => codes::INVALID_PARTY,
            Self::InvalidTopic { .. } => codes::INVALID_TOPIC,
            Self::InvalidGrant { .. } => codes::INVALID_GRANT,
            Self::InvalidSchemaRange { .. } => codes::INVALID_SCHEMA_RANGE,
            Self::InvalidField { .. } => codes::INVALID_FIELD,
            Self::FieldClassification { .. } => codes::FIELD_CLASSIFICATION,
            Self::InvalidOffer { .. } => codes::INVALID_OFFER,
            Self::InvalidExpiry { .. } => codes::INVALID_EXPIRY,
            Self::ExternalRecordTopic { .. } => codes::EXTERNAL_RECORD_TOPIC,
            Self::NotFound { .. } => codes::NOT_FOUND,
            Self::VersionConflict { .. } => codes::VERSION_CONFLICT,
            Self::DuplicateLink { .. } => codes::DUPLICATE_LINK,
            Self::InvalidTransition { .. } => codes::INVALID_TRANSITION,
            Self::NotActive { .. } => codes::NOT_ACTIVE,
            Self::Revoked { .. } => codes::REVOKED,
            Self::LinkExpired { .. } => codes::LINK_EXPIRED,
            Self::GrantExpired { .. } => codes::GRANT_EXPIRED,
            Self::TopicNotGranted { .. } => codes::TOPIC_NOT_GRANTED,
            Self::WrongDirection { .. } => codes::WRONG_DIRECTION,
            Self::OfferConflict { .. } => codes::OFFER_CONFLICT,
            Self::GrantWider { .. } => codes::GRANT_WIDER,
            Self::RegionRefused { .. } => codes::REGION_REFUSED,
            Self::PeerRegionMismatch { .. } => codes::PEER_REGION_MISMATCH,
            Self::BadSignature => codes::BAD_SIGNATURE,
            Self::WrongSigner { .. } => codes::WRONG_SIGNER,
            Self::KeyMismatch { .. } => codes::KEY_MISMATCH,
            Self::KeyUnpinned { .. } => codes::KEY_UNPINNED,
            Self::Replayed { .. } => codes::REPLAYED,
            Self::OutOfOrder { .. } => codes::OUT_OF_ORDER,
            Self::TranscriptMismatch => codes::TRANSCRIPT_MISMATCH,
            Self::WrongStep { .. } => codes::WRONG_STEP,
            Self::WrongLink { .. } => codes::WRONG_LINK,
            Self::ConsentRequired { .. } => codes::CONSENT_REQUIRED,
            Self::ConsentDuplicate { .. } => codes::CONSENT_DUPLICATE,
            Self::ConsentInvalid { .. } => codes::CONSENT_INVALID,
            Self::StepUpRequired { .. } => codes::STEP_UP_REQUIRED,
            Self::ConsentStale { .. } => codes::CONSENT_STALE,
            Self::AuditFailed { .. } => codes::AUDIT_FAILED,
            Self::StoreFailed { .. } => codes::STORE_FAILED,
        }
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;
