//! One stream entry, parsed: the envelope, its class and, for a state entry, the operation, the
//! schema and the image.

use serde_json::Value;
use spot_events_contract::envelope::{EventEnvelope, Position};

use crate::error::RebuildError;
use crate::options::{EventClass, classify};

/// The net operation of a `doc_state` entry (ADR-061 s3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Insert,
    Update,
    Delete,
}

/// The payload of a `doc_state` entry.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) op: Op,
    pub(crate) version: u64,
    /// The catalog version the image was written under (`0` for a delete without one).
    pub(crate) entity_schema: u64,
    /// The whole document; `Null` for a delete that carries none.
    pub(crate) image: Value,
}

/// One parsed entry.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) position: Position,
    pub(crate) class: EventClass,
    pub(crate) topic: String,
    pub(crate) event_id: String,
    pub(crate) entity: String,
    pub(crate) id: String,
    /// Present for a state entry.
    pub(crate) state: Option<State>,
    /// The topic data of an audit entry (taken out of the envelope); `Null` otherwise.
    pub(crate) data: Value,
}

/// Parses one segment line at `position` and checks it belongs to `source` and to that position.
pub(crate) fn parse(line: &str, position: Position, source: &str) -> Result<Entry, RebuildError> {
    let invalid = |reason: &'static str| RebuildError::EntryInvalid {
        generation: position.generation,
        seq: position.seq,
        reason,
    };
    let mut envelope =
        EventEnvelope::from_json(line).map_err(|_| invalid("the line is not an event envelope"))?;
    if envelope.source_id.as_str() != source {
        return Err(RebuildError::ForeignEntry {
            generation: position.generation,
            seq: position.seq,
        });
    }
    if envelope.position != position {
        return Err(invalid("the envelope names another position than its slot"));
    }
    let topic = envelope.topic.as_str().to_owned();
    let class = classify(&topic);
    let entity = envelope.subject.entity.clone();
    let id = envelope.subject.id.to_string();
    let event_id = envelope.event_id.to_string();
    let data = std::mem::take(&mut envelope.data);
    let (state, data) = match class {
        EventClass::State => {
            let version = envelope
                .subject_version
                .ok_or_else(|| invalid("a state entry has no subject version"))?;
            (Some(parse_state(data, version, &invalid)?), Value::Null)
        }
        EventClass::Audit => (None, data),
        EventClass::Domain => (None, Value::Null),
    };
    Ok(Entry {
        position,
        class,
        topic,
        event_id,
        entity,
        id,
        state,
        data,
    })
}

fn parse_state(
    data: Value,
    version: u64,
    invalid: &dyn Fn(&'static str) -> RebuildError,
) -> Result<State, RebuildError> {
    let Value::Object(mut fields) = data else {
        return Err(invalid("a state entry's data is not an object"));
    };
    let op = match fields.get("op").and_then(Value::as_str) {
        Some("insert") => Op::Insert,
        Some("update") => Op::Update,
        Some("delete") => Op::Delete,
        _ => {
            return Err(invalid(
                "a state entry's op is not insert, update or delete",
            ));
        }
    };
    let schema = fields.get("entity_schema").and_then(Value::as_u64);
    let image = fields.remove("image").unwrap_or(Value::Null);
    if op == Op::Delete {
        return Ok(State {
            op,
            version,
            entity_schema: schema.unwrap_or(0),
            image: Value::Null,
        });
    }
    let Some(entity_schema) = schema else {
        return Err(invalid("a state entry has no entity_schema"));
    };
    if !image.is_object() {
        return Err(invalid("a state entry's image is not an object"));
    }
    Ok(State {
        op,
        version,
        entity_schema,
        image,
    })
}
