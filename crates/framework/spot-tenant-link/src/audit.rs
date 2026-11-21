//! Audit: every state change and every use of a link writes an entry on the local side.
//!
//! The entries are `spot-audit-chain` records: the same [`AuditRecord`] shape every other
//! tamper-evident trail stores, so an adapter over the chain (or its store) is all the wiring
//! there is. This crate keeps the port synchronous and sans-I/O; a host adds the chain and its
//! checkpoints.
//!
//! Both sides of a link reconstruct the same milestones independently ([`reconstruct`]): each
//! holds its own local records, and the shared events carry the link id, the event and both
//! actors' consent entries in the payload.
//!
//! No payload holds a business value, a field value or a secret: the link id, the event, the
//! revision, a fixed detail word and, for a consent, the actor.

use std::sync::Mutex;

use spot_audit_chain::{AuditRecord, Payload, PayloadValue, Subject};

use crate::error::{Error, Result};
use crate::ids::LinkId;

/// What happened to a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LinkEventKind {
    /// One side proposed the link.
    Proposed,
    /// The peer accepted.
    Accepted,
    /// A side published its identity key.
    KeyExchanged,
    /// Both consents, both keys and the grants are in place.
    Activated,
    /// The peer rejected the proposal.
    Rejected,
    /// A side revoked the link.
    Revoked,
    /// The link reached its expiry.
    Expired,
    /// A sender or receiver used the link and was allowed.
    Allowed,
    /// A sender or receiver used the link and was refused.
    Refused,
    /// A change was audited but its store write failed.
    ChangeFailed,
}

impl LinkEventKind {
    /// The event as text, also the suffix of the audit record's kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Accepted => "accepted",
            Self::KeyExchanged => "key_exchanged",
            Self::Activated => "activated",
            Self::Rejected => "rejected",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::Allowed => "allowed",
            Self::Refused => "refused",
            Self::ChangeFailed => "change_failed",
        }
    }

    /// The audit record's dotted kind.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Proposed => "tenant_link.proposed",
            Self::Accepted => "tenant_link.accepted",
            Self::KeyExchanged => "tenant_link.key_exchanged",
            Self::Activated => "tenant_link.activated",
            Self::Rejected => "tenant_link.rejected",
            Self::Revoked => "tenant_link.revoked",
            Self::Expired => "tenant_link.expired",
            Self::Allowed => "tenant_link.allowed",
            Self::Refused => "tenant_link.refused",
            Self::ChangeFailed => "tenant_link.change_failed",
        }
    }

    fn parse(kind: &str) -> Option<Self> {
        Some(match kind {
            "tenant_link.proposed" => Self::Proposed,
            "tenant_link.accepted" => Self::Accepted,
            "tenant_link.key_exchanged" => Self::KeyExchanged,
            "tenant_link.activated" => Self::Activated,
            "tenant_link.rejected" => Self::Rejected,
            "tenant_link.revoked" => Self::Revoked,
            "tenant_link.expired" => Self::Expired,
            "tenant_link.allowed" => Self::Allowed,
            "tenant_link.refused" => Self::Refused,
            "tenant_link.change_failed" => Self::ChangeFailed,
            _ => return None,
        })
    }
}

/// One audit-worthy thing that happened to a link, on this side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkEvent {
    /// The link.
    pub link: LinkId,
    /// What happened.
    pub kind: LinkEventKind,
    /// Who acted (`system`, `user:<id>` or `agent:<id>`).
    pub actor: String,
    /// Unix milliseconds.
    pub at: u64,
    /// The handshake revision or the stored version the event belongs to.
    pub revision: u64,
    /// A fixed detail, such as the refused error code or a consent actor.
    pub detail: Option<&'static str>,
}

impl LinkEvent {
    /// The event as the audit chain's record.
    ///
    /// # Errors
    /// [`Error::InvalidId`] when the instant does not fit the chain's signed time.
    pub fn to_audit_record(&self) -> Result<AuditRecord> {
        if !actor_ok(&self.actor) {
            return Err(Error::InvalidId {
                reason: "an audit actor is `system`, `user:<id>` or `agent:<id>`",
            });
        }
        let at = i64::try_from(self.at).map_err(|_| Error::InvalidId {
            reason: "the audit instant is out of range",
        })?;
        let mut payload = Payload::new();
        payload.insert("link".to_owned(), PayloadValue::Text(self.link.to_string()));
        payload.insert(
            "event".to_owned(),
            PayloadValue::Text(self.kind.as_str().to_owned()),
        );
        payload.insert(
            "revision".to_owned(),
            PayloadValue::Int(i64::try_from(self.revision).unwrap_or(i64::MAX)),
        );
        if let Some(detail) = self.detail {
            payload.insert("detail".to_owned(), PayloadValue::Text(detail.to_owned()));
        }
        Ok(AuditRecord {
            kind: self.kind.code().to_owned(),
            actor: self.actor.clone(),
            on_behalf_of: None,
            subject: Some(Subject {
                doctype: "tenant_link".to_owned(),
                id: self.link.to_string(),
            }),
            at,
            payload,
            masked: true,
        })
    }

    /// Reads one event back from a chain record; `None` for a record of another kind or shape.
    #[must_use]
    pub fn from_audit_record(record: &AuditRecord) -> Option<Self> {
        let kind = LinkEventKind::parse(&record.kind)?;
        let link = record
            .payload
            .get("link")
            .and_then(|value| match value {
                PayloadValue::Text(text) => Some(text.as_str()),
                _ => None,
            })?
            .parse()
            .ok()?;
        let revision = record
            .payload
            .get("revision")
            .and_then(|value| match value {
                PayloadValue::Int(value) => u64::try_from(*value).ok(),
                _ => None,
            })
            .unwrap_or(0);
        let detail = record
            .payload
            .get("detail")
            .and_then(|value| match value {
                PayloadValue::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .and_then(interned);
        Some(Self {
            link,
            kind,
            actor: record.actor.clone(),
            at: u64::try_from(record.at).ok()?,
            revision,
            detail,
        })
    }
}

fn actor_ok(actor: &str) -> bool {
    !actor.is_empty()
        && actor.len() <= 256
        && (actor == "system"
            || actor
                .strip_prefix("user:")
                .or_else(|| actor.strip_prefix("agent:"))
                .is_some_and(|id| !id.is_empty() && !id.contains(char::is_whitespace)))
}

/// Only the fixed detail words this crate emits are accepted back, so nothing free-form can
/// enter a reconstructed event.
fn interned(text: &str) -> Option<&'static str> {
    const DETAILS: [&str; 51] = [
        crate::error::codes::INVALID_ID,
        crate::error::codes::INVALID_PARTY,
        crate::error::codes::INVALID_TOPIC,
        crate::error::codes::INVALID_GRANT,
        crate::error::codes::INVALID_SCHEMA_RANGE,
        crate::error::codes::INVALID_FIELD,
        crate::error::codes::FIELD_CLASSIFICATION,
        crate::error::codes::INVALID_OFFER,
        crate::error::codes::INVALID_EXPIRY,
        crate::error::codes::EXTERNAL_RECORD_TOPIC,
        crate::error::codes::NOT_FOUND,
        crate::error::codes::VERSION_CONFLICT,
        crate::error::codes::DUPLICATE_LINK,
        crate::error::codes::INVALID_TRANSITION,
        crate::error::codes::NOT_ACTIVE,
        crate::error::codes::REVOKED,
        crate::error::codes::LINK_EXPIRED,
        crate::error::codes::GRANT_EXPIRED,
        crate::error::codes::TOPIC_NOT_GRANTED,
        crate::error::codes::WRONG_DIRECTION,
        crate::error::codes::OFFER_CONFLICT,
        crate::error::codes::GRANT_WIDER,
        crate::error::codes::REGION_REFUSED,
        crate::error::codes::PEER_REGION_MISMATCH,
        crate::error::codes::BAD_SIGNATURE,
        crate::error::codes::WRONG_SIGNER,
        crate::error::codes::KEY_MISMATCH,
        crate::error::codes::KEY_UNPINNED,
        crate::error::codes::REPLAYED,
        crate::error::codes::OUT_OF_ORDER,
        crate::error::codes::TRANSCRIPT_MISMATCH,
        crate::error::codes::WRONG_STEP,
        crate::error::codes::WRONG_LINK,
        crate::error::codes::CONSENT_REQUIRED,
        crate::error::codes::CONSENT_DUPLICATE,
        crate::error::codes::CONSENT_INVALID,
        crate::error::codes::STEP_UP_REQUIRED,
        crate::error::codes::CONSENT_STALE,
        crate::error::codes::AUDIT_FAILED,
        crate::error::codes::STORE_FAILED,
        "send",
        "receive",
        "both",
        "terms_declined",
        "grants_mismatch",
        "keys_unacceptable",
        "terms_ended",
        "security_concern",
        "superseded",
        "message",
        "record",
    ];
    DETAILS.iter().find(|code| **code == text).copied()
}

/// The audit port: an adapter over the local `spot-audit-chain` chain.
pub trait AuditSink: Send + Sync {
    /// Appends one record, returning its chain sequence number.
    ///
    /// # Errors
    /// [`Error::AuditFailed`] when the record could not be made durable.
    fn append(&self, record: AuditRecord) -> Result<u64>;
}

/// Writes one event and maps any sink failure to [`Error::AuditFailed`].
///
/// # Errors
/// [`Error::AuditFailed`] naming `action`.
pub fn record(sink: &dyn AuditSink, event: &LinkEvent, action: &'static str) -> Result<()> {
    let record = event.to_audit_record()?;
    sink.append(record)
        .map(|_| ())
        .map_err(|_| Error::AuditFailed { action })
}

/// An in-memory [`AuditSink`]: the reference for tests and for a synchronous host.
#[derive(Default)]
pub struct MemoryAuditLog {
    records: Mutex<Vec<AuditRecord>>,
}

impl MemoryAuditLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record, in append order.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the lock is poisoned.
    pub fn entries(&self) -> Result<Vec<AuditRecord>> {
        self.records
            .lock()
            .map(|records| records.clone())
            .map_err(|_| Error::StoreFailed {
                reason: "the audit log lock is poisoned",
            })
    }

    /// The events this crate wrote, in append order.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the lock is poisoned.
    pub fn events(&self) -> Result<Vec<LinkEvent>> {
        Ok(reconstruct(&self.entries()?))
    }
}

impl AuditSink for MemoryAuditLog {
    fn append(&self, record: AuditRecord) -> Result<u64> {
        let mut records = self.records.lock().map_err(|_| Error::StoreFailed {
            reason: "the audit log lock is poisoned",
        })?;
        records.push(record);
        u64::try_from(records.len()).map_err(|_| Error::StoreFailed {
            reason: "the audit log is too long",
        })
    }
}

/// Rebuilds the link events from a chain's records, in chain order.
///
/// This is what either side runs over its own trail to show the history; the two sides compare
/// the shared milestones.
#[must_use]
pub fn reconstruct(records: &[AuditRecord]) -> Vec<LinkEvent> {
    records
        .iter()
        .filter_map(LinkEvent::from_audit_record)
        .collect()
}
