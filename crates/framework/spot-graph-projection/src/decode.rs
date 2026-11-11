//! Decoding a committed outbox record into an edge event.
//!
//! The projection reads the **outbox** (ADR-11, ADR-053): one committed [`OutboxRecord`] per
//! event, in publication order. The graph index (WP-GRA-02) leaves the decoder to this crate:
//! the record's feed **position** becomes the event's `sequence`, and the record's payload
//! becomes the rest of the [`EdgeEvent`]. The payload topic and schema are not yet in
//! `contracts/events` registry: the edge fact is `sys_graph_edge` ([`EDGE_TOPIC`], the registry
//! constant) and the retired name `doc_edge` ([`RETIRED_EDGE_TOPIC`]) is still read as an alias,
//! because the journal is immutable and old entries carry it. Both carry the same payload and
//! decode to the same event; [`decode_record_with_topic`] also says which name was read.
//!
//! A record on another topic is not an edge event ([`decode_record`] returns `None`), so a caller
//! may feed the whole feed and let the projector advance its checkpoint past everything it sees.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_graph_index::{EdgeEvent, EdgeKey};
use spot_storage_contract::{OutboxRecord, Value};

use crate::error::ProjectionError;
use crate::wire::{graph_from_json, node_from_json, op_from_str, properties_from_json};

/// The outbox topic that carries one change to one business-graph edge: the registered name.
pub const EDGE_TOPIC: &str = spot_events_contract::system::GRAPH_EDGE_TOPIC;

/// The retired name of the same fact (`retired` in the events registry). It is read, never
/// written, so that journals and feeds already written still project. Remove it only after the
/// retention horizon has passed and [`Projector::retired_alias_reads`](crate::Projector) stays 0.
pub const RETIRED_EDGE_TOPIC: &str = "doc_edge";

/// Which name of the edge topic a record carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeTopic {
    /// The registered name, `sys_graph_edge`.
    Registered,
    /// The retired alias, `doc_edge`.
    RetiredAlias,
}

impl EdgeTopic {
    /// The topic name this variant stands for.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Registered => EDGE_TOPIC,
            Self::RetiredAlias => RETIRED_EDGE_TOPIC,
        }
    }

    /// Classifies a topic name; `None` for any other topic.
    #[must_use]
    pub fn of(topic: &str) -> Option<Self> {
        if topic == EDGE_TOPIC {
            Some(Self::Registered)
        } else if topic == RETIRED_EDGE_TOPIC {
            Some(Self::RetiredAlias)
        } else {
            None
        }
    }
}

/// The payload schema version this decoder understands.
pub const EDGE_SCHEMA_VERSION: u32 = 1;

/// Decodes one committed record into an edge event.
///
/// `Ok(None)` means the record is not an edge event (another topic) and the caller should skip it
/// while still advancing its checkpoint. `Err` means the record claims to be an edge event but
/// its payload is malformed; the projector reports it and moves on rather than blocking the feed.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when the schema version is not supported or the payload is not a
/// well-formed edge event.
pub fn decode_record(record: &OutboxRecord) -> Result<Option<EdgeEvent>, ProjectionError> {
    decode_record_with_topic(record).map(|decoded| decoded.map(|(event, _)| event))
}

/// As [`decode_record`], and says which name of the edge topic the record carried, so the caller
/// can count reads of the retired alias.
///
/// # Errors
///
/// As [`decode_record`].
pub fn decode_record_with_topic(
    record: &OutboxRecord,
) -> Result<Option<(EdgeEvent, EdgeTopic)>, ProjectionError> {
    let Some(topic) = EdgeTopic::of(record.event.topic.as_str()) else {
        return Ok(None);
    };
    let event = decode_payload(record, topic)?;
    Ok(Some((event, topic)))
}

fn decode_payload(record: &OutboxRecord, topic: EdgeTopic) -> Result<EdgeEvent, ProjectionError> {
    if record.event.schema_version != EDGE_SCHEMA_VERSION {
        return Err(ProjectionError::decode(format!(
            "the `{}` schema version {} is not supported",
            topic.name(),
            record.event.schema_version
        )));
    }
    let payload: Json = serde_json::from_str(&record.event.payload)
        .map_err(|e| ProjectionError::decode(format!("the payload is not JSON: {e}")))?;
    let object = payload
        .as_object()
        .ok_or_else(|| ProjectionError::decode("the payload is not an object"))?;
    let graph = graph_from_json(object.get("graph"))?;
    let op = object
        .get("op")
        .and_then(Json::as_str)
        .ok_or_else(|| ProjectionError::decode("the payload has no operation"))
        .and_then(op_from_str)?;
    let edge_type = object
        .get("edge_type")
        .and_then(Json::as_str)
        .ok_or_else(|| ProjectionError::decode("the payload has no edge type"))?;
    let from = node_from_json(
        object
            .get("from")
            .ok_or_else(|| ProjectionError::decode("the payload has no `from`"))?,
    )?;
    let to = node_from_json(
        object
            .get("to")
            .ok_or_else(|| ProjectionError::decode("the payload has no `to`"))?,
    )?;
    let properties: BTreeMap<String, Value> = match object.get("properties") {
        Some(raw) => properties_from_json(raw)?,
        None => BTreeMap::new(),
    };
    Ok(EdgeEvent {
        sequence: record.position.seq(),
        graph,
        op,
        edge: EdgeKey::new(edge_type, from, to),
        properties,
    })
}
