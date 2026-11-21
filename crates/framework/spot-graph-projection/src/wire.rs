//! The JSON-lines wire form of the projection: one edge event or one snapshot line per event.
//!
//! The reference [`FileSink`](crate::FileSink) writes these lines for tests and for export, and
//! [`replay`](crate::replay) reads them back. The outbox payload of an edge event
//! ([`decode_record`](crate::decode_record)) uses the same value encoding, so a payload and a
//! wire line agree.
//!
//! Storage values have no `serde` form yet (the index crate says so), so each [`Value`] is
//! encoded as a **tagged** JSON object `{"t": <tag>, "v": <payload>}`. The tag keeps a text
//! value, a JSON value, a date and a decimal apart, so a round trip is exact and a decimal is
//! never turned into a float (ADR-12). A record id is its 16 bytes as an array of numbers, and a
//! name is its text; both round-trip without a UUID dependency.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};
use spot_graph_index::{Edge, EdgeEvent, EdgeKey, EdgeOp, GraphKey};
use spot_storage_contract::{Name, NodeRef, RecordId, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

use crate::error::ProjectionError;

/// The wire-form version. A line of any other version is refused.
pub const WIRE_VERSION: u64 = 1;

/// The JSON key carrying the line kind (`event`, `edge` or `snapshot`).
pub const KIND: &str = "kind";

fn tagged(tag: &str, payload: Json) -> Json {
    let mut object = Map::new();
    object.insert("t".to_owned(), Json::String(tag.to_owned()));
    object.insert("v".to_owned(), payload);
    Json::Object(object)
}

fn decode(reason: impl Into<String>) -> ProjectionError {
    ProjectionError::decode(reason)
}

/// Encodes one storage value as its tagged JSON form.
#[must_use]
pub fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => tagged("null", Json::Null),
        Value::Bool(b) => tagged("bool", Json::Bool(*b)),
        Value::Int(i) => tagged("int", Json::from(*i)),
        Value::Decimal(d) => tagged("decimal", Json::String(d.to_string())),
        Value::Text(text) => tagged("text", Json::String(text.clone())),
        Value::Date(date) => tagged(
            "date",
            Json::String(
                date.format(&format_description!("[year]-[month]-[day]"))
                    .unwrap_or_default(),
            ),
        ),
        Value::Time(clock) => tagged(
            "time",
            Json::String(
                clock
                    .format(&format_description!(
                        "[hour]:[minute]:[second].[subsecond digits:9]"
                    ))
                    .unwrap_or_default(),
            ),
        ),
        Value::DateTime(instant) => tagged(
            "datetime",
            Json::String(instant.format(&Rfc3339).unwrap_or_default()),
        ),
        Value::Id(id) => tagged("id", id_to_json(id)),
        Value::Json(text) => tagged("json", Json::String(text.clone())),
        Value::Bytes(bytes) => tagged(
            "bytes",
            Json::Array(bytes.iter().map(|byte| Json::from(*byte)).collect()),
        ),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1) and `value_from_json` has no
        // tag for them, so a projection that meets one fails to decode it, loudly.
        Value::GeoPoint(_) | Value::Range(_) => {
            tagged("unsupported", Json::String(format!("{value:?}")))
        }
    }
}

/// Decodes a tagged JSON value back into a storage value.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when the object is not a tagged value or its payload does not fit
/// the tag.
pub fn value_from_json(value: &Json) -> Result<Value, ProjectionError> {
    let object = value
        .as_object()
        .ok_or_else(|| decode("a value is not a tagged object"))?;
    let tag = object
        .get("t")
        .and_then(Json::as_str)
        .ok_or_else(|| decode("a value has no type tag"))?;
    let payload = object
        .get("v")
        .ok_or_else(|| decode("a value has no payload"))?;
    match tag {
        "null" => Ok(Value::Null),
        "bool" => payload
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| decode("a bool value is not a boolean")),
        "int" => payload
            .as_i64()
            .map(Value::Int)
            .ok_or_else(|| decode("an int value is not an integer")),
        "decimal" => {
            let text = payload
                .as_str()
                .ok_or_else(|| decode("a decimal value is not a string"))?;
            text.parse::<rust_decimal::Decimal>()
                .map(Value::Decimal)
                .map_err(|e| decode(format!("a decimal value is malformed: {e}")))
        }
        "text" => payload
            .as_str()
            .map(|text| Value::Text(text.to_owned()))
            .ok_or_else(|| decode("a text value is not a string")),
        "date" => {
            let text = payload
                .as_str()
                .ok_or_else(|| decode("a date value is not a string"))?;
            time::Date::parse(text, &format_description!("[year]-[month]-[day]"))
                .map(Value::Date)
                .map_err(|e| decode(format!("a date value is malformed: {e}")))
        }
        "time" => {
            let text = payload
                .as_str()
                .ok_or_else(|| decode("a time value is not a string"))?;
            time::Time::parse(
                text,
                &format_description!("[hour]:[minute]:[second].[subsecond digits:9]"),
            )
            .map(Value::Time)
            .map_err(|e| decode(format!("a time value is malformed: {e}")))
        }
        "datetime" => {
            let text = payload
                .as_str()
                .ok_or_else(|| decode("a datetime value is not a string"))?;
            OffsetDateTime::parse(text, &Rfc3339)
                .map(Value::DateTime)
                .map_err(|e| decode(format!("a datetime value is malformed: {e}")))
        }
        "id" => id_from_json(payload).map(Value::Id),
        "json" => payload
            .as_str()
            .map(|text| Value::Json(text.to_owned()))
            .ok_or_else(|| decode("a json value is not a string")),
        "bytes" => {
            let items = payload
                .as_array()
                .ok_or_else(|| decode("a bytes value is not an array"))?;
            let mut bytes = Vec::with_capacity(items.len());
            for item in items {
                let byte = item
                    .as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| decode("a bytes value holds a non-byte"))?;
                bytes.push(byte);
            }
            Ok(Value::Bytes(bytes))
        }
        other => Err(decode(format!("unknown value tag `{other}`"))),
    }
}

fn id_to_json(id: &RecordId) -> Json {
    Json::Array(id.as_bytes().iter().map(|byte| Json::from(*byte)).collect())
}

fn id_from_json(value: &Json) -> Result<RecordId, ProjectionError> {
    let items = value
        .as_array()
        .ok_or_else(|| decode("a record id is not an array"))?;
    let mut bytes = [0u8; 16];
    if items.len() != bytes.len() {
        return Err(decode(format!(
            "a record id has {} bytes, not 16",
            items.len()
        )));
    }
    for (slot, item) in bytes.iter_mut().zip(items) {
        *slot = item
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .ok_or_else(|| decode("a record id holds a non-byte"))?;
    }
    RecordId::from_bytes(bytes).map_err(|e| decode(format!("a record id is invalid: {e}")))
}

/// Encodes a record key as `{"entity": <name>, "id": [<16 bytes>]}`.
#[must_use]
pub fn node_to_json(node: &NodeRef) -> Json {
    let mut object = Map::new();
    object.insert(
        "entity".to_owned(),
        Json::String(node.entity.as_str().to_owned()),
    );
    object.insert("id".to_owned(), id_to_json(&node.id));
    Json::Object(object)
}

/// Decodes a record key.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when the entity is not a valid storage name or the id is malformed.
pub fn node_from_json(value: &Json) -> Result<NodeRef, ProjectionError> {
    let object = value
        .as_object()
        .ok_or_else(|| decode("a record key is not an object"))?;
    let entity = object
        .get("entity")
        .and_then(Json::as_str)
        .ok_or_else(|| decode("a record key has no entity"))?;
    let entity = Name::new(entity).map_err(|e| decode(format!("a record key entity: {e}")))?;
    let id = object
        .get("id")
        .ok_or_else(|| decode("a record key has no id"))
        .and_then(id_from_json)?;
    Ok(NodeRef { entity, id })
}

/// Encodes a graph key as `{"tenant": ..., "graph": ...}`.
#[must_use]
pub fn graph_to_json(key: &GraphKey) -> Json {
    let mut object = Map::new();
    object.insert("tenant".to_owned(), Json::String(key.tenant.clone()));
    object.insert("graph".to_owned(), Json::String(key.graph.clone()));
    Json::Object(object)
}

/// Decodes a graph key.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when the tenant or graph name is missing.
pub fn graph_from_json(value: Option<&Json>) -> Result<GraphKey, ProjectionError> {
    let object = value
        .and_then(Json::as_object)
        .ok_or_else(|| decode("a graph key is not an object"))?;
    let tenant = object
        .get("tenant")
        .and_then(Json::as_str)
        .ok_or_else(|| decode("a graph key has no tenant"))?;
    let graph = object
        .get("graph")
        .and_then(Json::as_str)
        .ok_or_else(|| decode("a graph key has no graph name"))?;
    Ok(GraphKey::new(tenant, graph))
}

/// The stable text tag of an edge operation.
#[must_use]
pub fn op_to_str(op: EdgeOp) -> &'static str {
    op.as_str()
}

/// Parses an edge operation's text tag.
///
/// # Errors
///
/// [`ProjectionError::Decode`] for an unknown tag.
pub fn op_from_str(text: &str) -> Result<EdgeOp, ProjectionError> {
    match text {
        "add" => Ok(EdgeOp::Add),
        "remove" => Ok(EdgeOp::Remove),
        "update" => Ok(EdgeOp::Update),
        other => Err(decode(format!("unknown edge operation `{other}`"))),
    }
}

/// Encodes a property map as a JSON object of tagged values.
#[must_use]
pub fn properties_to_json(properties: &BTreeMap<String, Value>) -> Json {
    let mut object = Map::new();
    for (name, value) in properties {
        object.insert(name.clone(), value_to_json(value));
    }
    Json::Object(object)
}

/// Decodes a property map.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when a property value is malformed.
pub fn properties_from_json(value: &Json) -> Result<BTreeMap<String, Value>, ProjectionError> {
    let object = value
        .as_object()
        .ok_or_else(|| decode("the properties are not an object"))?;
    let mut properties = BTreeMap::new();
    for (name, raw) in object {
        properties.insert(name.clone(), value_from_json(raw)?);
    }
    Ok(properties)
}

/// Encodes one edge as `{"edge_type", "from", "to", "properties"}`.
#[must_use]
pub fn edge_to_json(edge: &Edge) -> Json {
    let mut object = Map::new();
    object.insert(KIND.to_owned(), Json::String("edge".to_owned()));
    object.insert(
        "edge_type".to_owned(),
        Json::String(edge.key.edge_type.clone()),
    );
    object.insert("from".to_owned(), node_to_json(&edge.key.from));
    object.insert("to".to_owned(), node_to_json(&edge.key.to));
    object.insert(
        "properties".to_owned(),
        properties_to_json(&edge.properties),
    );
    Json::Object(object)
}

/// Decodes one edge line.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when a field is missing or malformed.
pub fn edge_from_json(value: &Json) -> Result<Edge, ProjectionError> {
    let object = value
        .as_object()
        .ok_or_else(|| decode("an edge line is not an object"))?;
    let edge_type = object
        .get("edge_type")
        .and_then(Json::as_str)
        .ok_or_else(|| decode("an edge line has no edge type"))?;
    let from = node_from_json(
        object
            .get("from")
            .ok_or_else(|| decode("an edge line has no `from`"))?,
    )?;
    let to = node_from_json(
        object
            .get("to")
            .ok_or_else(|| decode("an edge line has no `to`"))?,
    )?;
    let properties = match object.get("properties") {
        Some(raw) => properties_from_json(raw)?,
        None => BTreeMap::new(),
    };
    Ok(Edge {
        key: EdgeKey::new(edge_type, from, to),
        properties,
    })
}

/// Encodes one edge event as a full wire line.
#[must_use]
pub fn event_to_json(event: &EdgeEvent) -> Json {
    let mut object = Map::new();
    object.insert(KIND.to_owned(), Json::String("event".to_owned()));
    object.insert("v".to_owned(), Json::from(WIRE_VERSION));
    object.insert("seq".to_owned(), Json::from(event.sequence));
    object.insert("graph".to_owned(), graph_to_json(&event.graph));
    object.insert(
        "op".to_owned(),
        Json::String(op_to_str(event.op).to_owned()),
    );
    object.insert(
        "edge_type".to_owned(),
        Json::String(event.edge.edge_type.clone()),
    );
    object.insert("from".to_owned(), node_to_json(&event.edge.from));
    object.insert("to".to_owned(), node_to_json(&event.edge.to));
    object.insert(
        "properties".to_owned(),
        properties_to_json(&event.properties),
    );
    Json::Object(object)
}

/// Decodes one edge event line.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when a field is missing or malformed.
pub fn event_from_json(value: &Json) -> Result<EdgeEvent, ProjectionError> {
    let object = value
        .as_object()
        .ok_or_else(|| decode("an edge event line is not an object"))?;
    let sequence = object
        .get("seq")
        .and_then(Json::as_u64)
        .ok_or_else(|| decode("an edge event line has no sequence"))?;
    let graph = graph_from_json(object.get("graph"))?;
    let op = object
        .get("op")
        .and_then(Json::as_str)
        .ok_or_else(|| decode("an edge event line has no operation"))
        .and_then(op_from_str)?;
    let edge_type = object
        .get("edge_type")
        .and_then(Json::as_str)
        .ok_or_else(|| decode("an edge event line has no edge type"))?;
    let from = node_from_json(
        object
            .get("from")
            .ok_or_else(|| decode("an edge event line has no `from`"))?,
    )?;
    let to = node_from_json(
        object
            .get("to")
            .ok_or_else(|| decode("an edge event line has no `to`"))?,
    )?;
    let properties = match object.get("properties") {
        Some(raw) => properties_from_json(raw)?,
        None => BTreeMap::new(),
    };
    Ok(EdgeEvent {
        sequence,
        graph,
        op,
        edge: EdgeKey::new(edge_type, from, to),
        properties,
    })
}

/// Encodes a snapshot header: the graph and the watermark the following edge lines describe.
#[must_use]
pub fn snapshot_to_json(key: &GraphKey, watermark: u64) -> Json {
    let mut object = Map::new();
    object.insert(KIND.to_owned(), Json::String("snapshot".to_owned()));
    object.insert("v".to_owned(), Json::from(WIRE_VERSION));
    object.insert("graph".to_owned(), graph_to_json(key));
    object.insert("watermark".to_owned(), Json::from(watermark));
    Json::Object(object)
}

/// The kind of a wire line, or [`ProjectionError::Decode`] for an unknown one.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when the line has no `kind` or an unknown one.
pub fn kind_of(value: &Json) -> Result<&'static str, ProjectionError> {
    let kind = value
        .get(KIND)
        .and_then(Json::as_str)
        .ok_or_else(|| decode("a wire line has no kind"))?;
    match kind {
        "event" => Ok("event"),
        "edge" => Ok("edge"),
        "snapshot" => Ok("snapshot"),
        other => Err(decode(format!("unknown wire line kind `{other}`"))),
    }
}

/// The watermark a snapshot header carries.
///
/// # Errors
///
/// [`ProjectionError::Decode`] when the watermark is missing.
pub fn snapshot_watermark(value: &Json) -> Result<u64, ProjectionError> {
    value
        .get("watermark")
        .and_then(Json::as_u64)
        .ok_or_else(|| decode("a snapshot line has no watermark"))
}
