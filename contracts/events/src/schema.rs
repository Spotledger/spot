//! Payload schemas: what a topic's `data` contains, per major version.
//!
//! Types follow the native API value encoding (ADR-055), so an event payload and an API
//! response encode a decimal, a date or an id the same way. Validation is **strict**: it is
//! what producers and tests run, so an unknown field, a JSON number for a decimal or a null in a
//! required field is an error. Consumers, by contrast, must ignore fields they do not know.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use time::format_description::well_known::Rfc3339;

use crate::names::Topic;

/// The type of one payload field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayloadType {
    /// JSON boolean.
    Bool,
    /// JSON integer in the signed 64-bit range.
    Int,
    /// Exact decimal as a string, `^-?[0-9]+(\.[0-9]+)?$`; never a JSON number.
    Decimal,
    /// JSON string.
    Text,
    /// `YYYY-MM-DD`.
    Date,
    /// RFC 3339 in UTC, ending in `Z`.
    DateTime,
    /// A record id: canonical lower-case hyphenated UUIDv7.
    Id,
    /// Any JSON value.
    Json,
    /// A closed or open set of string values. `open` declares that new values may be added
    /// within a major version, so consumers must handle values they do not know.
    Enum {
        /// Allowed values.
        values: BTreeSet<String>,
        /// Whether values may be added without a new major version.
        open: bool,
    },
    /// A JSON array of one element type.
    List(Box<PayloadType>),
    /// A nested JSON object with its own fields, under the same rules as the top level.
    Object(BTreeMap<String, FieldSchema>),
}

/// One field of a payload or nested object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldSchema {
    /// The field's type.
    pub ty: PayloadType,
    /// Required fields are always present and never null; optional fields may be absent or null.
    pub required: bool,
    /// Marked for removal in the next major version. Producers keep sending it until then.
    pub deprecated: bool,
}

impl FieldSchema {
    /// A required field.
    pub fn required(ty: PayloadType) -> Self {
        Self {
            ty,
            required: true,
            deprecated: false,
        }
    }

    /// An optional field.
    pub fn optional(ty: PayloadType) -> Self {
        Self {
            ty,
            required: false,
            deprecated: false,
        }
    }
}

/// The payload schema of one topic at one major version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventSchema {
    /// The topic.
    pub topic: Topic,
    /// The major version, from 1: `schema_version` on the wire.
    pub major: u32,
    /// The revision within the major version, from 0; recorded in the registry only.
    pub revision: u32,
    /// Top-level fields of `data`.
    pub fields: BTreeMap<String, FieldSchema>,
    /// Field names removed in earlier major versions; never reused (ADR-056 rule 3).
    pub retired: BTreeSet<String>,
}

/// One validation failure, with the JSON path it happened at (for example `lines[2].qty`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaError {
    /// Where, as a path from `data`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl EventSchema {
    /// Validates `data` strictly against the schema, reporting every problem.
    pub fn validate(&self, data: &Json) -> Result<(), Vec<SchemaError>> {
        let mut errors = Vec::new();
        match data.as_object() {
            Some(obj) => check_object(&self.fields, obj, "", &mut errors),
            None => errors.push(err("", "data must be a JSON object")),
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Checks the schema's own rules: `major` from 1, field names are storage-style
    /// (`[a-z][a-z0-9_]*`), no field uses a retired name, and enums are not empty.
    pub fn check_definition(&self) -> Result<(), Vec<SchemaError>> {
        let mut errors = Vec::new();
        if self.major == 0 {
            errors.push(err("", "major versions start at 1"));
        }
        check_definition_fields(&self.fields, "", &mut errors);
        for name in self.fields.keys() {
            if self.retired.contains(name) {
                errors.push(err(name, "a retired field name may not be reused"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn err(path: &str, message: &str) -> SchemaError {
    SchemaError {
        path: path.to_owned(),
        message: message.to_owned(),
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn valid_field_name(n: &str) -> bool {
    let mut b = n.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_lowercase())
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && n.len() <= 63
}

fn check_definition_fields(
    fields: &BTreeMap<String, FieldSchema>,
    path: &str,
    errors: &mut Vec<SchemaError>,
) {
    for (name, f) in fields {
        let p = join(path, name);
        if !valid_field_name(name) {
            errors.push(err(&p, "field names are [a-z][a-z0-9_]*, at most 63 bytes"));
        }
        check_definition_type(&f.ty, &p, errors);
    }
}

fn check_definition_type(ty: &PayloadType, path: &str, errors: &mut Vec<SchemaError>) {
    match ty {
        PayloadType::Enum { values, .. } if values.is_empty() => {
            errors.push(err(path, "an enum needs values"))
        }
        PayloadType::List(inner) => check_definition_type(inner, &format!("{path}[]"), errors),
        PayloadType::Object(fields) => check_definition_fields(fields, path, errors),
        _ => {}
    }
}

fn check_object(
    fields: &BTreeMap<String, FieldSchema>,
    obj: &serde_json::Map<String, Json>,
    path: &str,
    errors: &mut Vec<SchemaError>,
) {
    for (name, f) in fields {
        let p = join(path, name);
        match obj.get(name) {
            None | Some(Json::Null) if f.required => {
                errors.push(err(&p, "required field is missing or null"))
            }
            None | Some(Json::Null) => {}
            Some(v) => check_value(&f.ty, v, &p, errors),
        }
    }
    for name in obj.keys() {
        if !fields.contains_key(name) {
            errors.push(err(&join(path, name), "unknown field"));
        }
    }
}

fn check_value(ty: &PayloadType, v: &Json, path: &str, errors: &mut Vec<SchemaError>) {
    let ok = match ty {
        PayloadType::Bool => v.is_boolean(),
        PayloadType::Int => v.is_i64(),
        PayloadType::Decimal => v.as_str().is_some_and(is_decimal),
        PayloadType::Text => v.is_string(),
        PayloadType::Date => v.as_str().is_some_and(is_date),
        PayloadType::DateTime => v.as_str().is_some_and(is_utc_datetime),
        PayloadType::Id => v
            .as_str()
            .is_some_and(|s| crate::payload::record_id::parse(s).is_ok()),
        PayloadType::Json => true,
        PayloadType::Enum { values, .. } => v.as_str().is_some_and(|s| values.contains(s)),
        PayloadType::List(inner) => match v.as_array() {
            Some(items) => {
                for (i, item) in items.iter().enumerate() {
                    check_value(inner, item, &format!("{path}[{i}]"), errors);
                }
                true
            }
            None => false,
        },
        PayloadType::Object(fields) => match v.as_object() {
            Some(obj) => {
                check_object(fields, obj, path, errors);
                true
            }
            None => false,
        },
    };
    if !ok {
        errors.push(err(path, &format!("expected {}", describe(ty))));
    }
}

fn describe(ty: &PayloadType) -> &'static str {
    match ty {
        PayloadType::Bool => "a boolean",
        PayloadType::Int => "a 64-bit integer",
        PayloadType::Decimal => "a decimal string such as \"10.50\" (never a JSON number)",
        PayloadType::Text => "a string",
        PayloadType::Date => "a date YYYY-MM-DD",
        PayloadType::DateTime => "an RFC 3339 UTC time ending in Z",
        PayloadType::Id => "a canonical UUIDv7 record id",
        PayloadType::Json => "any JSON",
        PayloadType::Enum { .. } => "one of the enum's values",
        PayloadType::List(_) => "an array",
        PayloadType::Object(_) => "an object",
    }
}

/// `^-?[0-9]+(\.[0-9]+)?$`
pub(crate) fn is_decimal(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

pub(crate) fn is_date(s: &str) -> bool {
    let fmt = time::macros::format_description!("[year]-[month]-[day]");
    s.len() == 10 && time::Date::parse(s, &fmt).is_ok()
}

pub(crate) fn is_utc_datetime(s: &str) -> bool {
    s.ends_with('Z') && time::OffsetDateTime::parse(s, &Rfc3339).is_ok()
}
