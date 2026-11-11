//! The JSON Schema subset a message payload is checked against (ADR-080
//! section 5.2, ADR-058 rule 6).
//!
//! A message's [`Field`] list is the whole truth: the schema is *derived* from
//! it, so the list of fields that may cross and the shape a payload must have
//! cannot drift apart. The subset is deliberately small:
//!
//! - the payload is a JSON **object**;
//! - each property is a string (optionally bounded by `maxLength`), an integer
//!   (money and quantities as minor units, ADR-12) or a boolean;
//! - `additionalProperties` is always `false`: a field not in the contract does
//!   not cross, not even as an unknown key;
//! - there are no floats, no `number`, no arrays and no nested objects.
//!
//! [`Schema::to_json`] renders the subset as the standard JSON Schema object
//! the send and receive handlers check against; [`Schema::validate`] performs
//! the same check without a JSON Schema library, so the crate stays dependency
//! free and deterministic.

use serde_json::Value;

use crate::model::{Field, FieldKind, MessageDef};

/// One property of a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaProperty {
    /// The property's name.
    pub name: String,
    /// The kind of value it carries.
    pub kind: FieldKind,
    /// The longest text, for a string property.
    pub max_len: Option<u64>,
}

/// The schema of one message's payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// The message the schema belongs to.
    pub message: String,
    /// Every property the payload may carry.
    pub properties: Vec<SchemaProperty>,
    /// The properties the payload must carry.
    pub required: Vec<String>,
}

impl MessageDef {
    /// The schema of this message, derived from its field list.
    #[must_use]
    pub fn schema(&self) -> Schema {
        Schema {
            message: self.name.clone(),
            properties: self
                .fields
                .iter()
                .map(|field| SchemaProperty {
                    name: field.name.clone(),
                    kind: field.kind,
                    max_len: field.max_len,
                })
                .collect(),
            required: self
                .fields
                .iter()
                .filter(|field: &&Field| field.required)
                .map(|field| field.name.clone())
                .collect(),
        }
    }
}

impl Schema {
    /// The schema as a standard JSON Schema object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut properties = serde_json::Map::new();
        for property in &self.properties {
            let mut spec = serde_json::Map::new();
            spec.insert(
                "type".to_owned(),
                Value::String(property.kind.as_str().to_owned()),
            );
            if let Some(max_len) = property.max_len {
                spec.insert("maxLength".to_owned(), Value::Number(max_len.into()));
            }
            properties.insert(property.name.clone(), Value::Object(spec));
        }
        let mut schema = serde_json::Map::new();
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        schema.insert("additionalProperties".to_owned(), Value::Bool(false));
        schema.insert("properties".to_owned(), Value::Object(properties));
        if !self.required.is_empty() {
            schema.insert(
                "required".to_owned(),
                Value::Array(
                    self.required
                        .iter()
                        .map(|name| Value::String(name.clone()))
                        .collect(),
                ),
            );
        }
        Value::Object(schema)
    }

    /// Checks a payload against the schema, reporting **every** problem.
    ///
    /// The reasons are short and stable: they name the field and the rule, never
    /// the value, because a payload may carry a personal value and a reason
    /// goes into a log.
    #[must_use]
    pub fn validate(&self, payload: &Value) -> Vec<SchemaError> {
        let Some(object) = payload.as_object() else {
            return vec![SchemaError::payload("the payload is not a JSON object")];
        };
        let mut errors = Vec::new();
        for key in object.keys() {
            if !self.properties.iter().any(|p| &p.name == key) {
                errors.push(SchemaError::field(
                    key,
                    "the payload carries a field the contract does not allow",
                ));
            }
        }
        for property in &self.properties {
            let Some(value) = object.get(&property.name) else {
                if self.required.iter().any(|name| name == &property.name) {
                    errors.push(SchemaError::field(
                        &property.name,
                        "a required field is missing",
                    ));
                }
                continue;
            };
            match (property.kind, value) {
                (FieldKind::String, Value::String(text)) => {
                    if let Some(max_len) = property.max_len
                        && u64::try_from(text.chars().count()).unwrap_or(u64::MAX) > max_len
                    {
                        errors.push(SchemaError::field(
                            &property.name,
                            "a text value is longer than the contract allows",
                        ));
                    }
                }
                (FieldKind::Integer, Value::Number(number)) if number.as_i64().is_some() => {}
                (FieldKind::Boolean, Value::Bool(_)) => {}
                (FieldKind::String, _) => {
                    errors.push(SchemaError::field(&property.name, "the value is not text"))
                }
                (FieldKind::Integer, _) => errors.push(SchemaError::field(
                    &property.name,
                    "the value is not an integer",
                )),
                (FieldKind::Boolean, _) => errors.push(SchemaError::field(
                    &property.name,
                    "the value is not a boolean",
                )),
            }
        }
        errors
    }
}

/// One reason a payload does not fit its message's schema.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchemaError {
    /// The field the problem anchors on; empty for a problem with the payload
    /// as a whole.
    pub field: String,
    /// A short, stable reason that names no value.
    pub reason: &'static str,
}

impl SchemaError {
    fn field(field: &str, reason: &'static str) -> Self {
        Self {
            field: field.to_owned(),
            reason,
        }
    }

    fn payload(reason: &'static str) -> Self {
        Self {
            field: String::new(),
            reason,
        }
    }
}
