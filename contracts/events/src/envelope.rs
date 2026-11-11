//! The external event envelope: what consumers outside the source database receive.
//!
//! The relay (WP-STO-08) reads [`OutboxRecord`]s from the outbox feed and turns each into an
//! [`EventEnvelope`], stamping the [`SourceId`] of the database it reads from. Integrations,
//! webhooks and branch sync (ADR-23) see only envelopes. Consumers deduplicate by `event_id`,
//! order per subject by `subject_version`, and never compare `position`, `transaction` or
//! versions across different `source_id`s.

use serde::{Deserialize, Serialize};
use spot_storage_contract::{FeedPosition, Name, NodeRef, OutboxRecord, RecordId, Version};

use crate::names::{SourceId, Topic};
use crate::payload::{EventMeta, PayloadDocument, PayloadError, record_id};

/// A published event as seen outside its source database. Serialised as one JSON object whose
/// field names are part of the contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    /// The event id: the deduplication key.
    #[serde(with = "record_id")]
    pub event_id: RecordId,
    /// The topic.
    pub topic: Topic,
    /// The payload's major schema version.
    pub schema_version: u32,
    /// The database that produced the event.
    pub source_id: SourceId,
    /// Position in that source's outbox feed.
    pub position: Position,
    /// The business transaction that enqueued the event, as assigned by the source.
    #[serde(with = "record_id")]
    pub transaction: RecordId,
    /// Enqueue order within that transaction, from 0.
    pub ordinal: u32,
    /// The document the event is about.
    pub subject: Subject,
    /// The subject's revision, when the event comes from a document write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_version: Option<u64>,
    /// Metadata.
    pub meta: EventMeta,
    /// Topic data.
    pub data: serde_json::Value,
}

/// A feed position in JSON form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    /// The stream generation.
    pub generation: u64,
    /// The sequence number within the generation.
    pub seq: u64,
}

/// The subject in JSON form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    /// The entity (DocType storage name).
    pub entity: String,
    /// The record id.
    #[serde(with = "record_id")]
    pub id: RecordId,
}

/// Why an envelope could not be built or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The outbox event's payload is not a valid payload document.
    Payload(PayloadError),
    /// The JSON text is not a valid envelope.
    Malformed(String),
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeError::Payload(e) => e.fmt(f),
            EnvelopeError::Malformed(m) => write!(f, "malformed event envelope: {m}"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

impl EventEnvelope {
    /// Builds the envelope for a published outbox record from `source`.
    pub fn from_record(source: &SourceId, record: &OutboxRecord) -> Result<Self, EnvelopeError> {
        let doc =
            PayloadDocument::from_outbox_event(&record.event).map_err(EnvelopeError::Payload)?;
        let topic = Topic::any(record.event.topic.as_str())
            .map_err(|e| EnvelopeError::Malformed(e.to_string()))?;
        Ok(Self {
            event_id: record.event.id,
            topic,
            schema_version: record.event.schema_version,
            source_id: source.clone(),
            position: Position {
                generation: record.position.generation(),
                seq: record.position.seq(),
            },
            transaction: record.transaction,
            ordinal: record.ordinal,
            subject: Subject {
                entity: record.event.subject.entity.as_str().to_owned(),
                id: record.event.subject.id,
            },
            subject_version: record.event.subject_version.map(|v| v.0),
            meta: doc.meta,
            data: doc.data,
        })
    }

    /// The feed position as the storage type.
    pub fn feed_position(&self) -> FeedPosition {
        FeedPosition::new(self.position.generation, self.position.seq)
    }

    /// The subject as the storage type; fails if the entity is not a storage name.
    pub fn subject_ref(&self) -> Result<NodeRef, EnvelopeError> {
        let entity =
            Name::new(&self.subject.entity).map_err(|e| EnvelopeError::Malformed(e.to_string()))?;
        Ok(NodeRef {
            entity,
            id: self.subject.id,
        })
    }

    /// The subject version as the storage type.
    pub fn subject_version(&self) -> Option<Version> {
        self.subject_version.map(Version)
    }

    /// The canonical JSON text.
    pub fn to_json(&self) -> Result<String, EnvelopeError> {
        serde_json::to_string(self).map_err(|e| EnvelopeError::Malformed(e.to_string()))
    }

    /// Parses and checks an envelope.
    pub fn from_json(text: &str) -> Result<Self, EnvelopeError> {
        let env: EventEnvelope =
            serde_json::from_str(text).map_err(|e| EnvelopeError::Malformed(e.to_string()))?;
        env.subject_ref()?;
        if !env.data.is_object() {
            return Err(EnvelopeError::Payload(PayloadError::DataNotObject));
        }
        Ok(env)
    }
}
