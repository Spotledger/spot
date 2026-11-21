//! JSON test input to documents: the native API's value encoding, by the field's kind. A
//! decimal is a string (`"1.50"`) or a number, a date `YYYY-MM-DD`, an instant RFC 3339, a link
//! the hyphenated id text, a child table an array of objects.

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use serde_json::Value as Json;
use spot_app_api::document::{ChildRow, Document, DocumentPatch, RowPatch};
use spot_app_api::meta::{DocTypeDef, FieldKind};
use spot_app_api::prelude::{Date, Month, Name, OffsetDateTime, RecordId, Time, Value};
use spot_meta::EffectiveMeta;
use time::format_description::well_known::Rfc3339;

fn name(text: &str) -> std::result::Result<Name, String> {
    Name::new(text).map_err(|e| format!("`{text}` is not a field name: {e}"))
}

/// Parses an id written as a hyphenated UUID.
pub fn parse_id(text: &str) -> std::result::Result<RecordId, String> {
    let digits: String = text.chars().filter(|c| *c != '-').collect();
    if digits.len() != 32 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("`{text}` is not a record id"));
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("`{text}` is not a record id"))?;
    }
    RecordId::from_bytes(bytes).map_err(|_| format!("`{text}` is not a version 7 record id"))
}

fn scalar(kind: &FieldKind, value: &Json) -> std::result::Result<Value, String> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let text = || {
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "expected a string".to_owned())
    };
    match kind {
        FieldKind::Bool => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| "expected a boolean".to_owned()),
        FieldKind::Int { .. } => value
            .as_i64()
            .map(Value::Int)
            .ok_or_else(|| "expected an integer".to_owned()),
        FieldKind::Decimal { .. } => {
            let source = match value {
                Json::String(s) => s.clone(),
                Json::Number(n) => n.to_string(),
                _ => return Err("expected a decimal as a string or a number".to_owned()),
            };
            Decimal::from_str_exact(&source)
                .map(Value::Decimal)
                .map_err(|_| "expected an exact decimal".to_owned())
        }
        FieldKind::Text { .. } | FieldKind::LongText { .. } | FieldKind::Select { .. } => {
            text().map(Value::Text)
        }
        FieldKind::Date => parse_date(&text()?).map(Value::Date),
        FieldKind::Time => parse_time(&text()?).map(Value::Time),
        FieldKind::DateTime => OffsetDateTime::parse(&text()?, &Rfc3339)
            .map(Value::DateTime)
            .map_err(|_| "expected an RFC 3339 instant".to_owned()),
        FieldKind::Link { .. }
        | FieldKind::DynamicLink { .. }
        | FieldKind::Attach { .. }
        | FieldKind::ConsumerRef => parse_id(&text()?).map(Value::Id),
        FieldKind::Json { .. } => Ok(Value::Json(value.to_string())),
        _ => Err("this field kind is not set from test input".to_owned()),
    }
}

fn bad_date<E>(_error: E) -> String {
    "expected a date as YYYY-MM-DD".to_owned()
}

fn bad_time<E>(_error: E) -> String {
    "expected a time as HH:MM[:SS]".to_owned()
}

fn parse_date(text: &str) -> std::result::Result<Date, String> {
    let mut parts = text.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("expected a date as YYYY-MM-DD".to_owned());
    };
    let month = Month::try_from(m.parse::<u8>().map_err(bad_date)?).map_err(bad_date)?;
    Date::from_calendar_date(
        y.parse::<i32>().map_err(bad_date)?,
        month,
        d.parse::<u8>().map_err(bad_date)?,
    )
    .map_err(bad_date)
}

fn parse_time(text: &str) -> std::result::Result<Time, String> {
    let mut parts = text.split(':');
    let (Some(h), Some(m), s) = (parts.next(), parts.next(), parts.next()) else {
        return Err("expected a time as HH:MM[:SS]".to_owned());
    };
    Time::from_hms(
        h.parse::<u8>().map_err(bad_time)?,
        m.parse::<u8>().map_err(bad_time)?,
        s.map_or(Ok(0), str::parse::<u8>).map_err(bad_time)?,
    )
    .map_err(bad_time)
}

fn field_of<'a>(
    def: &'a DocTypeDef,
    field: &str,
) -> std::result::Result<&'a spot_app_api::meta::FieldDef, String> {
    def.fields
        .iter()
        .find(|f| f.name.as_str() == field)
        .ok_or_else(|| format!("`{}` has no field `{field}`", def.name))
}

fn row_values(
    child: &DocTypeDef,
    object: &serde_json::Map<String, Json>,
) -> std::result::Result<BTreeMap<Name, Value>, String> {
    let mut fields = BTreeMap::new();
    for (key, value) in object {
        let def = field_of(child, key)?;
        fields.insert(
            name(key)?,
            scalar(&def.kind, value).map_err(|e| format!("{}.{key}: {e}", child.name))?,
        );
    }
    Ok(fields)
}

fn child_def(
    meta: &EffectiveMeta,
    kind: &FieldKind,
) -> std::result::Result<Arc<DocTypeDef>, String> {
    let doctype = match kind {
        FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => doctype,
        _ => return Err("not a table".to_owned()),
    };
    meta.doctype(doctype.as_str())
        .ok_or_else(|| format!("the child DocType `{doctype}` is unknown"))
}

/// A new document of `def` with `id` from `input` (a JSON object), child rows numbered by
/// `next_id`.
pub(crate) fn document(
    meta: &EffectiveMeta,
    def: &DocTypeDef,
    id: RecordId,
    input: &Json,
    next_id: &mut dyn FnMut() -> RecordId,
) -> std::result::Result<Document, String> {
    let object = input
        .as_object()
        .ok_or("the document must be a JSON object")?;
    let mut doc = Document::new(def.name.clone(), id);
    for (key, value) in object {
        let field = field_of(def, key)?;
        if matches!(
            field.kind,
            FieldKind::Table { .. } | FieldKind::TableMultiselect { .. }
        ) {
            let child = child_def(meta, &field.kind)?;
            let rows = value
                .as_array()
                .ok_or_else(|| format!("{key}: expected an array of rows"))?;
            for row in rows {
                let object = row
                    .as_object()
                    .ok_or_else(|| format!("{key}: a row must be an object"))?;
                doc.rows_mut(name(key)?).push(ChildRow {
                    id: next_id(),
                    fields: row_values(&child, object)?,
                });
            }
        } else {
            doc.set(
                name(key)?,
                scalar(&field.kind, value).map_err(|e| format!("{key}: {e}"))?,
            );
        }
    }
    Ok(doc)
}

/// A merge patch of `def` from `input` (a JSON object).
pub(crate) fn patch(
    meta: &EffectiveMeta,
    def: &DocTypeDef,
    input: &Json,
) -> std::result::Result<DocumentPatch, String> {
    let object = input.as_object().ok_or("the patch must be a JSON object")?;
    let mut out = DocumentPatch::default();
    for (key, value) in object {
        let field = field_of(def, key)?;
        if matches!(
            field.kind,
            FieldKind::Table { .. } | FieldKind::TableMultiselect { .. }
        ) {
            let child = child_def(meta, &field.kind)?;
            let rows = value
                .as_array()
                .ok_or_else(|| format!("{key}: expected an array of rows"))?;
            let mut patches = Vec::new();
            for row in rows {
                let object = row
                    .as_object()
                    .ok_or_else(|| format!("{key}: a row must be an object"))?;
                patches.push(RowPatch {
                    id: None,
                    fields: row_values(&child, object)?,
                });
            }
            out.tables.insert(name(key)?, patches);
        } else {
            out.fields.insert(
                name(key)?,
                scalar(&field.kind, value).map_err(|e| format!("{key}: {e}"))?,
            );
        }
    }
    Ok(out)
}

/// The typed value of one field from JSON (for a list filter): the field's name and its value in
/// the field's kind.
pub(crate) fn field_value(
    def: &DocTypeDef,
    field: &str,
    value: &Json,
) -> std::result::Result<(Name, Value), String> {
    let def_field = field_of(def, field)?;
    if matches!(
        def_field.kind,
        FieldKind::Table { .. } | FieldKind::TableMultiselect { .. }
    ) {
        return Err(format!("`{field}` is a table and cannot be filtered on"));
    }
    Ok((
        name(field)?,
        scalar(&def_field.kind, value).map_err(|e| format!("{field}: {e}"))?,
    ))
}
