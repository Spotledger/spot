//! JSON Schema generation from field metadata, and JSON ↔ storage [`Value`] conversion.
//!
//! A tool's input schema is derived from the effective metadata: one property per readable
//! field, described by its label and help text, typed by its `FieldKind`. A hidden or
//! forbidden field is simply not a property, so the model never sees a field the principal
//! cannot use. Decimals travel as strings, never as JSON numbers, so no float ever appears
//! (ADR-12).

use serde_json::{Value as Json, json};
use spot_app_api::meta::{FieldDef, FieldKind, MaskFormat};
use spot_app_api::prelude::{Decimal, Value};
use spot_storage_contract::Name;

use crate::error::McpError;

/// The JSON Schema for one field.
#[must_use]
pub fn field_schema(field: &FieldDef) -> Json {
    let mut schema = kind_schema(&field.kind);
    let mut description = field.label.clone();
    if let Some(help) = &field.hints.description {
        description.push_str(" — ");
        description.push_str(help);
    }
    if let Some(object) = schema.as_object_mut() {
        object.insert("description".to_owned(), Json::String(description));
        if field.read_only {
            object.insert("readOnly".to_owned(), Json::Bool(true));
        }
    }
    schema
}

fn kind_schema(kind: &FieldKind) -> Json {
    match kind {
        FieldKind::Bool => json!({ "type": "boolean" }),
        FieldKind::Int { .. } => json!({ "type": "integer" }),
        // Decimals travel as strings so no float is ever involved (ADR-12).
        FieldKind::Decimal { scale, .. } => json!({
            "type": "string",
            "format": "decimal",
            "description": format!("an exact decimal with {scale} digits after the point"),
        }),
        FieldKind::Text { .. } | FieldKind::LongText { .. } => json!({ "type": "string" }),
        FieldKind::Select { values } => json!({ "type": "string", "enum": values }),
        FieldKind::Date => json!({ "type": "string", "format": "date" }),
        FieldKind::Time => json!({ "type": "string", "format": "time" }),
        FieldKind::DateTime => json!({ "type": "string", "format": "date-time" }),
        FieldKind::Link { doctype } => json!({
            "type": "string",
            "format": "record-id",
            "description": format!("the id of a `{doctype}` document"),
        }),
        FieldKind::DynamicLink { .. } => json!({ "type": "string", "format": "record-id" }),
        FieldKind::Attach { .. } => json!({ "type": "string", "format": "record-id" }),
        FieldKind::Json { .. } => json!({}),
        FieldKind::Bytes => json!({ "type": "string", "format": "byte" }),
        FieldKind::Secret => json!({
            "type": "null",
            "description": "a secret is set through the secrets service, never here",
        }),
        FieldKind::Table { .. } | FieldKind::TableMultiselect { .. } => json!({
            "type": "array",
            "items": { "type": "object" },
        }),
        _ => json!({ "type": "string" }),
    }
}

/// The JSON Schema for a set of fields, as a tool's `inputSchema`.
#[must_use]
pub fn tool_input_schema(fields: &[FieldDef]) -> Json {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for field in fields {
        properties.insert(field.name.as_str().to_owned(), field_schema(field));
        if field.required && !field.read_only {
            required.push(Json::String(field.name.as_str().to_owned()));
        }
    }
    json!({
        "type": "object",
        "properties": Json::Object(properties),
        "required": required,
        "additionalProperties": false,
    })
}

/// Converts one JSON value into a storage [`Value`] for `field`.
///
/// # Errors
///
/// [`McpError::invalid_arguments`] when the JSON does not fit the field's type; a secret is
/// always refused (it never travels in a document).
pub fn decode_value(field: &FieldDef, json: &Json) -> Result<Value, McpError> {
    if json.is_null() {
        return Ok(Value::Null);
    }
    let bad = || McpError::invalid_arguments(format!("field `{}` has the wrong type", field.name));
    Ok(match &field.kind {
        FieldKind::Bool => Value::Bool(json.as_bool().ok_or_else(bad)?),
        FieldKind::Int { .. } => Value::Int(json.as_i64().ok_or_else(bad)?),
        FieldKind::Decimal { .. } => {
            let text = json.as_str().ok_or_else(bad)?;
            let decimal = text.parse::<Decimal>().map_err(|_| bad())?;
            Value::Decimal(decimal)
        }
        FieldKind::Text { .. } | FieldKind::LongText { .. } | FieldKind::Select { .. } => {
            Value::Text(json.as_str().ok_or_else(bad)?.to_owned())
        }
        FieldKind::Date => {
            let text = json.as_str().ok_or_else(bad)?;
            let date =
                time::Date::parse(text, &time::format_description::well_known::Iso8601::DATE)
                    .map_err(|_| bad())?;
            Value::Date(date)
        }
        FieldKind::Time => {
            let text = json.as_str().ok_or_else(bad)?;
            let time =
                time::Time::parse(text, &time::format_description::well_known::Iso8601::TIME)
                    .map_err(|_| bad())?;
            Value::Time(time)
        }
        FieldKind::DateTime => {
            let text = json.as_str().ok_or_else(bad)?;
            let at =
                time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                    .map_err(|_| bad())?;
            Value::DateTime(at)
        }
        FieldKind::Link { .. } | FieldKind::DynamicLink { .. } | FieldKind::Attach { .. } => {
            let text = json.as_str().ok_or_else(bad)?;
            Value::Id(spot_http::api::codec::parse_record_id(text).ok_or_else(bad)?)
        }
        FieldKind::Json { .. } => Value::Json(
            serde_json::to_string(json).map_err(|_| McpError::internal("cannot encode json"))?,
        ),
        FieldKind::Bytes => Value::Bytes(json.as_str().ok_or_else(bad)?.as_bytes().to_vec()),
        FieldKind::Secret => {
            return Err(McpError::invalid_arguments(
                "a secret is set through the secrets service, never here",
            ));
        }
        FieldKind::Table { .. } | FieldKind::TableMultiselect { .. } => {
            return Err(McpError::invalid_arguments(
                "child tables are not accepted by this tool",
            ));
        }
        _ => return Err(bad()),
    })
}

/// Converts one storage [`Value`] into JSON, masking it when the caller asks.
#[must_use]
pub fn encode_value(value: &Value, mask: Option<&MaskFormat>) -> Json {
    if let Some(format) = mask {
        let text = value_text(value);
        return Json::String(spot_permissions::mask(&text, &permission_mask(format)));
    }
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::from(*n),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Date(d) => Json::String(d.to_string()),
        Value::Time(t) => Json::String(t.to_string()),
        Value::DateTime(d) => Json::String(d.to_string()),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Json(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Json::String(text.clone()))
        }
        Value::Bytes(bytes) => Json::String(String::from_utf8_lossy(bytes).into_owned()),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug form.
        Value::GeoPoint(_) | Value::Range(_) => Json::String(format!("{value:?}")),
    }
}

/// Converts one storage [`Value`] into JSON without a field definition (a loose encoding for
/// query rows, where the column's type is not carried).
#[must_use]
pub fn encode_value_loose(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::from(*n),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Date(d) => Json::String(d.to_string()),
        Value::Time(t) => Json::String(t.to_string()),
        Value::DateTime(d) => Json::String(d.to_string()),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Json(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Json::String(text.clone()))
        }
        Value::Bytes(bytes) => Json::String(String::from_utf8_lossy(bytes).into_owned()),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug form.
        Value::GeoPoint(_) | Value::Range(_) => Json::String(format!("{value:?}")),
    }
}

/// The permission crate's mask format, which mirrors the contract's.
#[must_use]
pub fn permission_mask(format: &MaskFormat) -> spot_permissions::MaskFormat {
    match format {
        MaskFormat::Placeholder => spot_permissions::MaskFormat::Placeholder,
        MaskFormat::LastChars(n) => spot_permissions::MaskFormat::LastChars(*n),
        _ => spot_permissions::MaskFormat::Placeholder,
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Text(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

/// A field's storage name.
#[must_use]
pub fn field_name(field: &FieldDef) -> Name {
    field.name.clone()
}
