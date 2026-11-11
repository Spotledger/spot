//! The record image on the wire: how a sender encodes a document version and how the apply path
//! decodes it.
//!
//! The sync protocol carries an opaque `payload` string ([`spot_sync_protocol::Record`]); WP-EDG-06
//! owns its shape. A payload is the JSON form of a [`RecordPayload`]: the root document, the
//! ledger and stock entries that travel with it, and the sender's metadata version. Every value
//! is **self-describing** ([`WireValue`]), so decoding needs no catalog; structural validation
//! against the receiver's catalog happens afterwards (ADR-062 G3, WP-EDG-06.T1).
//!
//! Values are encoded exactly (ADR-12): a decimal is its exact text, an instant is RFC 3339, and
//! nothing is ever a float.

use std::collections::BTreeMap;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use spot_storage_contract::{ChildRow, DocumentData, Fields, Name, RecordId, Value};
use time::format_description::FormatItem;
use time::macros::format_description;
use uuid::Uuid;

use crate::error::Reason;

/// The wire format of a calendar date.
const DATE_FORMAT: &[FormatItem<'static>] = format_description!("[year]-[month]-[day]");
/// The wire format of a wall-clock time: nine fractional digits, always.
const TIME_FORMAT: &[FormatItem<'static>] =
    format_description!("[hour]:[minute]:[second].[subsecond digits:9]");
/// The wire format of an instant: RFC 3339 with nine fractional digits and a mandatory offset.
const DATE_TIME_FORMAT: &[FormatItem<'static>] = format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]\
     [offset_hour sign:mandatory]:[offset_minute]"
);

/// One value in a record image. Self-describing, so a payload decodes without a catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum WireValue {
    /// No value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit integer.
    Int(i64),
    /// An exact decimal, as its text (never a float, ADR-12).
    Decimal(String),
    /// Text.
    Text(String),
    /// A calendar date, ISO 8601.
    Date(String),
    /// A wall-clock time, ISO 8601.
    Time(String),
    /// An instant, RFC 3339 in UTC.
    DateTime(String),
    /// A record reference, the UUIDv7 in its hyphenated text form.
    Id(String),
    /// A JSON document, as text.
    Json(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
}

impl WireValue {
    /// Converts to the storage value model.
    ///
    /// # Errors
    /// [`Reason::InvalidPayload`] when a tagged value is not in its documented form (a bad
    /// decimal, date, instant or id). The value itself is never echoed.
    pub fn to_value(&self) -> Result<Value, Reason> {
        match self {
            WireValue::Null => Ok(Value::Null),
            WireValue::Bool(b) => Ok(Value::Bool(*b)),
            WireValue::Int(i) => Ok(Value::Int(*i)),
            WireValue::Decimal(s) => rust_decimal::Decimal::from_str(s)
                .map(Value::Decimal)
                .map_err(|_| invalid("a decimal is not exact text")),
            WireValue::Text(s) => Ok(Value::Text(s.clone())),
            WireValue::Date(s) => time::Date::parse(s, DATE_FORMAT)
                .map(Value::Date)
                .map_err(|_| invalid("a date is not an ISO 8601 date")),
            WireValue::Time(s) => time::Time::parse(s, TIME_FORMAT)
                .map(Value::Time)
                .map_err(|_| invalid("a time is not an ISO 8601 time")),
            WireValue::DateTime(s) => time::OffsetDateTime::parse(s, DATE_TIME_FORMAT)
                .map(Value::DateTime)
                .map_err(|_| invalid("an instant is not an RFC 3339 timestamp")),
            WireValue::Id(s) => parse_id(s).map(Value::Id),
            WireValue::Json(s) => Ok(Value::Json(s.clone())),
            WireValue::Bytes(b) => Ok(Value::Bytes(b.clone())),
        }
    }

    /// Converts a storage value to its wire form.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => WireValue::Null,
            Value::Bool(b) => WireValue::Bool(*b),
            Value::Int(i) => WireValue::Int(*i),
            Value::Decimal(d) => WireValue::Decimal(d.to_string()),
            Value::Text(s) => WireValue::Text(s.clone()),
            Value::Date(d) => WireValue::Date(format_or_empty(d.format(DATE_FORMAT))),
            Value::Time(t) => WireValue::Time(format_or_empty(t.format(TIME_FORMAT))),
            Value::DateTime(t) => WireValue::DateTime(format_or_empty(t.format(DATE_TIME_FORMAT))),
            Value::Id(id) => WireValue::Id(id.to_string()),
            Value::Json(s) => WireValue::Json(s.clone()),
            Value::Bytes(b) => WireValue::Bytes(b.clone()),
            // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug text
            // until the wire format defines a form.
            Value::GeoPoint(_) | Value::Range(_) => WireValue::Text(format!("{value:?}")),
        }
    }
}

fn invalid(why: &str) -> Reason {
    Reason::InvalidPayload {
        why: why.to_owned(),
    }
}

/// Formats a value with a wire format. Formatting a valid date, time or instant with a format
/// it was built for cannot fail; the empty fallback is unreachable.
fn format_or_empty(result: Result<String, time::error::Format>) -> String {
    result.unwrap_or_default()
}

/// Parses a UUIDv7 in its hyphenated text form.
fn parse_id(text: &str) -> Result<RecordId, Reason> {
    let uuid = Uuid::parse_str(text).map_err(|_| Reason::InvalidId {
        id: "<not a uuid>".to_owned(),
    })?;
    RecordId::from_bytes(*uuid.as_bytes()).map_err(|_| Reason::InvalidId {
        id: uuid.to_string(),
    })
}

/// A field map on the wire.
pub type WireFields = BTreeMap<String, WireValue>;

/// One row of a child table on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildPayload {
    /// The row's record id, as hyphenated UUIDv7 text.
    pub id: String,
    /// Every declared field of the child entity.
    pub fields: WireFields,
}

/// One document on the wire: the root record or one of its ledger or stock entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocPayload {
    /// The document entity's storage name.
    pub entity: String,
    /// The document's record id, as hyphenated UUIDv7 text.
    pub id: String,
    /// Every declared field of the entity.
    pub fields: WireFields,
    /// Child rows per child-table field. A missing table means no rows.
    #[serde(default)]
    pub children: BTreeMap<String, Vec<ChildPayload>>,
}

impl DocPayload {
    /// Encodes a storage document as its wire form.
    #[must_use]
    pub fn from_document(doc: &DocumentData) -> Self {
        Self {
            entity: doc.entity.as_str().to_owned(),
            id: doc.id.to_string(),
            fields: doc
                .fields
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), WireValue::from_value(v)))
                .collect(),
            children: doc
                .children
                .iter()
                .map(|(table, rows)| {
                    let rows = rows
                        .iter()
                        .map(|row| ChildPayload {
                            id: row.id.to_string(),
                            fields: row
                                .fields
                                .iter()
                                .map(|(k, v)| (k.as_str().to_owned(), WireValue::from_value(v)))
                                .collect(),
                        })
                        .collect();
                    (table.as_str().to_owned(), rows)
                })
                .collect(),
        }
    }

    /// Decodes the wire form into a storage document.
    ///
    /// # Errors
    /// [`Reason::InvalidId`] for a bad id and [`Reason::InvalidPayload`] for a bad entity name
    /// or a value not in its documented form.
    pub fn to_document(&self) -> Result<DocumentData, Reason> {
        let entity = Name::new(&self.entity).map_err(|_| Reason::InvalidPayload {
            why: "an entity name is not a storage name".to_owned(),
        })?;
        let id = parse_id(&self.id)?;
        let fields = decode_fields(&self.fields)?;
        let mut children = BTreeMap::new();
        for (table, rows) in &self.children {
            let table = Name::new(table).map_err(|_| Reason::InvalidPayload {
                why: "a child-table name is not a storage name".to_owned(),
            })?;
            let mut decoded = Vec::with_capacity(rows.len());
            for row in rows {
                decoded.push(ChildRow {
                    id: parse_id(&row.id)?,
                    fields: decode_fields(&row.fields)?,
                });
            }
            children.insert(table, decoded);
        }
        Ok(DocumentData {
            entity,
            id,
            fields,
            children,
        })
    }
}

/// The whole record image: the root document, its ledger and stock entries, and the sender's
/// metadata version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordPayload {
    /// The root document at this version.
    pub document: DocPayload,
    /// The ledger and stock entries that came with it (ADR-023: a submitted version ships with
    /// the entries it produced).
    #[serde(default)]
    pub entries: Vec<DocPayload>,
    /// The sender's metadata version, for the skew check (ADR-066).
    pub meta_version: u64,
}

impl RecordPayload {
    /// The payload as the JSON text a [`Record`](spot_sync_protocol::Record) carries.
    ///
    /// # Errors
    /// [`Reason::InvalidPayload`] when serialisation fails (it cannot for these types).
    pub fn encode(&self) -> Result<String, Reason> {
        serde_json::to_string(self).map_err(|e| invalid(&e.to_string()))
    }

    /// Decodes a payload from its JSON text.
    ///
    /// # Errors
    /// [`Reason::InvalidPayload`] when the text is not a record image.
    pub fn decode(text: &str) -> Result<Self, Reason> {
        serde_json::from_str(text).map_err(|e| invalid(&e.to_string()))
    }
}

fn decode_fields(fields: &WireFields) -> Result<Fields, Reason> {
    let mut out = Fields::new();
    for (key, value) in fields {
        let name = Name::new(key).map_err(|_| Reason::InvalidPayload {
            why: "a field name is not a storage name".to_owned(),
        })?;
        out.insert(name, value.to_value()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::BTreeMap;
    use std::str::FromStr;

    #[test]
    fn every_value_round_trips() {
        let values = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(-7),
            Value::Decimal(rust_decimal::Decimal::from_str("12345.67").unwrap()),
            Value::Text("hello".to_owned()),
            Value::Date(time::Date::from_calendar_date(2026, time::Month::January, 2).unwrap()),
            Value::Time(time::Time::from_hms(3, 4, 5).unwrap()),
            Value::DateTime(time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()),
            Value::Id(RecordId::new_v7()),
            Value::Json("{\"a\":1}".to_owned()),
            Value::Bytes(vec![0, 1, 2, 255]),
        ];
        for value in values {
            let wire = WireValue::from_value(&value);
            assert_eq!(wire.to_value().unwrap(), value, "round trip {value:?}");
        }
    }

    #[test]
    fn a_non_v7_id_is_refused() {
        let nil = uuid::Uuid::nil().to_string();
        assert_eq!(
            WireValue::Id(nil).to_value().err().map(|r| r.code()),
            Some("replica_apply.invalid_id")
        );
    }

    #[test]
    fn a_payload_round_trips_with_children_and_entries() {
        let mut fields = Fields::new();
        fields.insert(Name::new("title").unwrap(), Value::Text("x".to_owned()));
        let mut children = BTreeMap::new();
        children.insert(
            Name::new("lines").unwrap(),
            vec![ChildRow {
                id: RecordId::new_v7(),
                fields: fields.clone(),
            }],
        );
        let doc = DocumentData {
            entity: Name::new("item").unwrap(),
            id: RecordId::new_v7(),
            fields,
            children,
        };
        let payload = RecordPayload {
            document: DocPayload::from_document(&doc),
            entries: vec![DocPayload::from_document(&doc)],
            meta_version: 3,
        };
        let text = payload.encode().unwrap();
        let decoded = RecordPayload::decode(&text).unwrap();
        assert_eq!(decoded, payload);
        assert_eq!(decoded.document.to_document().unwrap(), doc);
    }

    #[test]
    fn a_broken_payload_is_refused() {
        assert!(RecordPayload::decode("not json").is_err());
    }
}
