//! The event the planner reasons about: position, topic, subject, audience, payload.
//!
//! It mirrors the parts of the external envelope (ADR-056, `contracts/events`) the planner needs,
//! without depending on the contract: the planner never interprets a payload, so the payload here
//! is the source's canonical JSON **text**, opaque to this crate. The reference catalog that does
//! read it is a trait the host supplies ([`crate::References`]).

use serde::{Deserialize, Serialize};

/// A position in the source's event stream (ADR-056). Ordered, never compared across sources.
pub type Position = u64;

/// The sync identity of one record: its entity (DocType storage name) and its id.
///
/// ADR-064 calls this the record key. It is not the storage contract's opaque `RecordId`: this
/// crate may depend on nothing but `serde` and `thiserror`, so it carries the identity as text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RecordId {
    /// The entity (DocType storage name).
    pub entity: String,
    /// The record id.
    pub id: String,
}

impl RecordId {
    /// Build a record identity.
    #[must_use]
    pub fn new(entity: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            entity: entity.into(),
            id: id.into(),
        }
    }
}

/// An event's payload: the source's canonical JSON text (ADR-055), opaque to the planner.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(String);

impl Payload {
    /// Wrap a payload's text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The payload text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The payload's size in bytes, which is what a batch limit counts.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.0.len()
    }
}

/// The ADR-061 section 2 class of an event's topic.
///
/// The class is part of the topic's registered schema, so it is asked of the declarations
/// ([`crate::TopicDeclarations`]) rather than carried on the event: a topic cannot be planned under
/// a class it was not registered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EventClass {
    /// The full state of one record after one committed transaction.
    State,
    /// A business or app fact for consumers to react to.
    Domain,
    /// A system fact for proof or rebuild, sent to a node only when addressed to it.
    Audit,
}

/// One event in the gap: what the planner orders, coalesces and bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Position in the source's stream.
    pub position: Position,
    /// The topic, whose declarations fix the class (ADR-061 section 2).
    pub topic: String,
    /// The record the event is about. A state event always has one; a domain or audit event may not.
    pub subject: Option<RecordId>,
    /// The subject's revision, when the event describes a document version (ADR-056).
    pub subject_version: Option<u64>,
    /// The node this event is addressed to, for an audit event (ADR-064, ADR-063 adoption).
    pub addressed_to: Option<String>,
    /// Whether this event belongs to the mandatory lane (ADR-066 section 2): delivered
    /// individually, in order, ahead of all bulk, and never coalesced away.
    pub mandatory: bool,
    /// The payload: for a state event, the whole record image (ADR-061 section 3).
    pub payload: Payload,
}

impl Event {
    /// An event at `position` on `topic` with `payload`; add the rest with the `with_*` methods.
    #[must_use]
    pub fn new(position: Position, topic: impl Into<String>, payload: Payload) -> Self {
        Self {
            position,
            topic: topic.into(),
            subject: None,
            subject_version: None,
            addressed_to: None,
            mandatory: false,
            payload,
        }
    }

    /// The record this event is about.
    #[must_use]
    pub fn with_subject(mut self, record: RecordId) -> Self {
        self.subject = Some(record);
        self
    }

    /// The subject's revision.
    #[must_use]
    pub fn with_subject_version(mut self, version: u64) -> Self {
        self.subject_version = Some(version);
        self
    }

    /// The node this event is addressed to.
    #[must_use]
    pub fn with_addressee(mut self, node: impl Into<String>) -> Self {
        self.addressed_to = Some(node.into());
        self
    }

    /// Put the event in the mandatory lane.
    #[must_use]
    pub fn mandatory(mut self) -> Self {
        self.mandatory = true;
        self
    }

    /// The event's full image, when it names a record.
    ///
    /// The caller decides whether the event is a state event: this only carries the parts of an
    /// image (which record, which revision, the whole record) that dependency ordering needs.
    #[must_use]
    pub fn image(&self) -> Option<Image> {
        self.subject.clone().map(|record| Image {
            record,
            position: self.position,
            payload: self.payload.clone(),
        })
    }
}

/// A record's full image at one position: what dependency ordering is computed over (ADR-064
/// section 8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// Which record.
    pub record: RecordId,
    /// The position of the record's latest event: the deterministic tie-break.
    pub position: Position,
    /// The whole record, as the source's canonical JSON text.
    pub payload: Payload,
}

impl Image {
    /// Build an image.
    #[must_use]
    pub fn new(record: RecordId, position: Position, payload: Payload) -> Self {
        Self {
            record,
            position,
            payload,
        }
    }
}
