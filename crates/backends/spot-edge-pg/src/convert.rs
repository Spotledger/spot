//! Conversion between an [`Edge`] and a `spot_graph_edge` row.
//!
//! One row is one edge: its type id, its owner and its two ends as `uuid`/`text`, and its
//! properties as `jsonb`. The property map is an ordered projection of [`PropValue`], encoded
//! with an explicit tag per value so a decimal, an instant and a plain text value never collapse
//! into one another and nothing ever becomes a float (ADR-12).

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};
use spot_edge_sync::{Edge, PropValue};
use spot_graph_model::EdgeTypeId;
use spot_storage_contract::{Name, NodeRef, RecordId};
use uuid::Uuid;

use crate::error::{EdgePgError, Result};

/// One decoded `spot_graph_edge` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeRow {
    /// The edge type id, as its 64 hex characters.
    pub edge_type: String,
    /// The owner record's entity.
    pub owner_entity: String,
    /// The owner record's id.
    pub owner_id: Uuid,
    /// The source record's entity.
    pub from_entity: String,
    /// The source record's id.
    pub from_id: Uuid,
    /// The target record's entity.
    pub to_entity: String,
    /// The target record's id.
    pub to_id: Uuid,
    /// The properties, by field name.
    pub properties: Json,
}

/// The `uuid` a record id stores as.
#[must_use]
pub fn uuid_of(id: RecordId) -> Uuid {
    Uuid::from_bytes(*id.as_bytes())
}

/// The record id a stored `uuid` was.
pub fn record_id_of(id: Uuid) -> Result<RecordId> {
    RecordId::from_bytes(*id.as_bytes())
        .map_err(|_| EdgePgError::decode("a record id is not UUIDv7"))
}

/// The entity a stored name was.
pub fn entity_of(name: &str) -> Result<Name> {
    Name::new(name).map_err(|_| EdgePgError::decode(format!("`{name}` is not a storage name")))
}

/// The edge type id from its 64 hex characters.
pub fn edge_type_of(text: &str) -> Result<EdgeTypeId> {
    serde_json::from_value::<EdgeTypeId>(Json::String(text.to_owned()))
        .map_err(|_| EdgePgError::decode("an edge type id could not be read"))
}

/// One edge as a row.
pub fn edge_to_row(edge: &Edge) -> Result<EdgeRow> {
    Ok(EdgeRow {
        edge_type: edge.edge_type.as_str().to_owned(),
        owner_entity: edge.owner.entity.as_str().to_owned(),
        owner_id: uuid_of(edge.owner.id),
        from_entity: edge.from.entity.as_str().to_owned(),
        from_id: uuid_of(edge.from.id),
        to_entity: edge.to.entity.as_str().to_owned(),
        to_id: uuid_of(edge.to.id),
        properties: encode_properties(&edge.properties)?,
    })
}

/// One row as an edge.
pub fn row_to_edge(row: EdgeRow) -> Result<Edge> {
    Ok(Edge {
        edge_type: edge_type_of(&row.edge_type)?,
        owner: NodeRef {
            entity: entity_of(&row.owner_entity)?,
            id: record_id_of(row.owner_id)?,
        },
        from: NodeRef {
            entity: entity_of(&row.from_entity)?,
            id: record_id_of(row.from_id)?,
        },
        to: NodeRef {
            entity: entity_of(&row.to_entity)?,
            id: record_id_of(row.to_id)?,
        },
        properties: decode_properties(&row.properties)?,
    })
}

/// Encodes a property map as a JSON object of tagged values.
pub fn encode_properties(props: &BTreeMap<String, PropValue>) -> Result<Json> {
    let mut out = Map::new();
    for (name, value) in props {
        out.insert(name.clone(), encode_value(value));
    }
    Ok(Json::Object(out))
}

/// One property value as a tagged JSON value.
fn encode_value(value: &PropValue) -> Json {
    let (tag, body) = match value {
        PropValue::Null => ("null", Json::Null),
        PropValue::Bool(v) => ("bool", Json::Bool(*v)),
        PropValue::Int(v) => ("int", Json::from(*v)),
        PropValue::Decimal(v) => ("decimal", Json::String(v.clone())),
        PropValue::Text(v) => ("text", Json::String(v.clone())),
        PropValue::Date(v) => ("date", Json::String(v.clone())),
        PropValue::Time(v) => ("time", Json::String(v.clone())),
        PropValue::DateTime(v) => ("datetime", Json::String(v.clone())),
        PropValue::Id(v) => ("id", Json::String(uuid_of(*v).to_string())),
        PropValue::Json(v) => ("json", Json::String(v.clone())),
        PropValue::Bytes(v) => (
            "bytes",
            Json::Array(v.iter().map(|b| Json::from(*b)).collect()),
        ),
    };
    let mut obj = Map::new();
    obj.insert("t".to_owned(), Json::String(tag.to_owned()));
    obj.insert("v".to_owned(), body);
    Json::Object(obj)
}

/// Decodes a property map from a JSON object of tagged values.
pub fn decode_properties(value: &Json) -> Result<BTreeMap<String, PropValue>> {
    let Json::Object(map) = value else {
        return Err(EdgePgError::decode("edge properties are not a JSON object"));
    };
    let mut out = BTreeMap::new();
    for (name, encoded) in map {
        out.insert(name.clone(), decode_value(encoded)?);
    }
    Ok(out)
}

/// One tagged JSON value back to a [`PropValue`].
fn decode_value(value: &Json) -> Result<PropValue> {
    let Json::Object(obj) = value else {
        return Err(EdgePgError::decode(
            "an edge property is not a tagged object",
        ));
    };
    let tag = obj
        .get("t")
        .and_then(Json::as_str)
        .ok_or_else(|| EdgePgError::decode("an edge property has no tag"))?;
    let body = obj.get("v").unwrap_or(&Json::Null);
    match tag {
        "null" => Ok(PropValue::Null),
        "bool" => body
            .as_bool()
            .map(PropValue::Bool)
            .ok_or_else(|| EdgePgError::decode("a boolean property is not a boolean")),
        "int" => body
            .as_i64()
            .map(PropValue::Int)
            .ok_or_else(|| EdgePgError::decode("an integer property is not an integer")),
        "decimal" => text(body).map(PropValue::Decimal),
        "text" => text(body).map(PropValue::Text),
        "date" => text(body).map(PropValue::Date),
        "time" => text(body).map(PropValue::Time),
        "datetime" => text(body).map(PropValue::DateTime),
        "json" => text(body).map(PropValue::Json),
        "id" => {
            let raw = text(body)?;
            let uuid = Uuid::parse_str(&raw)
                .map_err(|_| EdgePgError::decode("a link property is not a uuid"))?;
            record_id_of(uuid).map(PropValue::Id)
        }
        "bytes" => {
            let Json::Array(items) = body else {
                return Err(EdgePgError::decode("a bytes property is not an array"));
            };
            let mut bytes = Vec::with_capacity(items.len());
            for item in items {
                let n = item
                    .as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| EdgePgError::decode("a bytes property holds a non-byte"))?;
                bytes.push(n);
            }
            Ok(PropValue::Bytes(bytes))
        }
        other => Err(EdgePgError::decode(format!(
            "`{other}` is not a known property tag"
        ))),
    }
}

fn text(value: &Json) -> Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| EdgePgError::decode("a text property is not text"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_storage_contract::{Name, RecordId};

    #[test]
    fn every_property_kind_round_trips_through_json() {
        let mut props: BTreeMap<String, PropValue> = BTreeMap::new();
        props.insert("null".to_owned(), PropValue::Null);
        props.insert("bool".to_owned(), PropValue::Bool(true));
        props.insert("int".to_owned(), PropValue::Int(-7));
        props.insert(
            "decimal".to_owned(),
            PropValue::Decimal("1.2300".to_owned()),
        );
        props.insert("text".to_owned(), PropValue::Text("héllo".to_owned()));
        props.insert("date".to_owned(), PropValue::Date("2026-10-17".to_owned()));
        props.insert("time".to_owned(), PropValue::Time("09:30:00".to_owned()));
        props.insert(
            "datetime".to_owned(),
            PropValue::DateTime("2026-10-17T09:30:00Z".to_owned()),
        );
        props.insert("id".to_owned(), PropValue::Id(RecordId::new_v7()));
        props.insert("json".to_owned(), PropValue::Json("{\"a\":1}".to_owned()));
        props.insert("bytes".to_owned(), PropValue::Bytes(vec![0, 1, 255]));

        let encoded = encode_properties(&props).unwrap();
        let decoded = decode_properties(&encoded).unwrap();
        assert_eq!(decoded, props);
    }

    #[test]
    fn an_edge_round_trips_through_a_row() {
        let edge = Edge::new(
            EdgeTypeId::of("invoice.party"),
            NodeRef {
                entity: Name::new("invoice").unwrap(),
                id: RecordId::new_v7(),
            },
            NodeRef {
                entity: Name::new("party").unwrap(),
                id: RecordId::new_v7(),
            },
        );
        let row = edge_to_row(&edge).unwrap();
        assert_eq!(row_to_edge(row).unwrap(), edge);
    }
}
