//! The document view a script sees, and the JSON the bridge carries in both directions.
//!
//! Money and quantities never cross the bridge as a float (ADR-12): a decimal is its exact
//! string, exactly as the generated `.d.ts` declares it (ADR-068 section 2). The view is built
//! through the host's [`FieldView`], so a script sees only what the triggering user may read.

use serde_json::{Map, Value as Json, json};
use spot_app_api::error::FieldPath;
use spot_app_api::meta::{DocTypeDef, FieldKind};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, Document, DocumentPatch, Invocation, RowPatch};
use time::{Date, Month, OffsetDateTime, Time};

/// Builds the document a script receives, through the same field access the API uses.
///
/// The host supplies the implementation: it decides, for the invocation's actor, which fields
/// are visible, which are masked and which are hidden. [`AllowAllView`] is the identity a test
/// or an unrestricted host uses.
pub trait FieldView: Send + Sync {
    /// The document as `inv`'s actor may read it.
    fn readable<'a>(
        &'a self,
        inv: &'a Invocation,
        def: &'a DocTypeDef,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<Document>>;
}

/// A view that hides nothing. Use it only where permissions are not enforced (tests, the system
/// principal); a real host supplies a view built on the site's authorizer.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllView;

impl FieldView for AllowAllView {
    fn readable<'a>(
        &'a self,
        _inv: &'a Invocation,
        _def: &'a DocTypeDef,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move { Ok(doc.clone()) })
    }
}

/// The JSON view of a document: the standard fields (`id`, `version`, `_state`) plus its own.
/// `project`, when given, keeps only those field and table names.
pub fn document_json(doc: &Document, project: Option<&[String]>) -> Json {
    let keep = |name: &str| project.is_none_or(|names| names.iter().any(|n| n.as_str() == name));
    let mut map = Map::new();
    map.insert("id".to_owned(), Json::String(doc.id().to_string()));
    if let Some(version) = doc.version() {
        map.insert("version".to_owned(), json!(version.0));
    }
    if let Some(state) = doc.state() {
        map.insert("_state".to_owned(), Json::String(state.as_str().to_owned()));
    }
    for (name, value) in doc.fields() {
        if keep(name.as_str()) {
            map.insert(name.as_str().to_owned(), value_json(value));
        }
    }
    for (table, rows) in doc.tables() {
        if !keep(table.as_str()) {
            continue;
        }
        let encoded: Vec<Json> = rows
            .iter()
            .map(|row| {
                let mut row_map = Map::new();
                row_map.insert("id".to_owned(), Json::String(row.id.to_string()));
                for (name, value) in &row.fields {
                    row_map.insert(name.as_str().to_owned(), value_json(value));
                }
                Json::Object(row_map)
            })
            .collect();
        map.insert(table.as_str().to_owned(), Json::Array(encoded));
    }
    Json::Object(map)
}

/// One stored value as JSON. A decimal is its exact string, never a float (ADR-12).
pub fn value_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => json!(i),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Date(d) => Json::String(format_date(*d)),
        Value::Time(t) => Json::String(format_time(*t)),
        Value::DateTime(dt) => Json::String(format_datetime(*dt)),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Json(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Json::String(text.clone()))
        }
        Value::Bytes(bytes) => Json::String(hex_encode(bytes)),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1). This function cannot fail,
        // so the value is shown in its exact debug form until the script API defines one.
        Value::GeoPoint(_) | Value::Range(_) => Json::String(format!("{value:?}")),
    }
}

/// One JSON value as a stored value of a field of kind `kind`.
///
/// # Errors
///
/// A message naming the value's shape, for the caller to report as a field error (a patch) or a
/// host error (`host.insert`/`host.update`).
pub fn json_to_value(kind: &FieldKind, value: &Json) -> Result<Value, String> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match kind {
        FieldKind::Bool => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| "expected true or false".to_owned()),
        FieldKind::Int { .. } => value
            .as_i64()
            .map(Value::Int)
            .ok_or_else(|| "expected an integer".to_owned()),
        FieldKind::Decimal { .. } => {
            let text = decimal_text(value)?;
            spot_money::rounding::parse_decimal(&text)
                .map(Value::Decimal)
                .map_err(|_| format!("`{text}` is not a decimal"))
        }
        FieldKind::Text { .. }
        | FieldKind::LongText { .. }
        | FieldKind::Select { .. }
        | FieldKind::DynamicLink { .. } => value
            .as_str()
            .map(|s| Value::Text(s.to_owned()))
            .ok_or_else(|| "expected a string".to_owned()),
        FieldKind::Date => value
            .as_str()
            .and_then(parse_date)
            .map(Value::Date)
            .ok_or_else(|| "expected a date, `YYYY-MM-DD`".to_owned()),
        FieldKind::Time => value
            .as_str()
            .and_then(parse_time)
            .map(Value::Time)
            .ok_or_else(|| "expected a time, `HH:MM:SS`".to_owned()),
        FieldKind::DateTime => parse_datetime(value)
            .map(Value::DateTime)
            .ok_or_else(|| "expected an RFC 3339 timestamp".to_owned()),
        FieldKind::Link { .. } | FieldKind::Attach { .. } => value
            .as_str()
            .and_then(parse_record_id)
            .map(Value::Id)
            .ok_or_else(|| "expected a record id".to_owned()),
        FieldKind::Json { .. } => Ok(Value::Json(value.to_string())),
        FieldKind::Bytes => value
            .as_str()
            .and_then(hex_decode)
            .map(Value::Bytes)
            .ok_or_else(|| "expected hex-encoded bytes".to_owned()),
        FieldKind::Secret => Err("a secret is set through the secrets service".to_owned()),
        FieldKind::Table { .. } | FieldKind::TableMultiselect { .. } => {
            Err("a table field holds an array of rows".to_owned())
        }
        _ => Err("this field kind is not writable from a script".to_owned()),
    }
}

/// A document patch from the JSON a script returned (`{ field: value, table: [ {..} ] }`).
///
/// # Errors
///
/// `document.unknown_field` for a name the DocType does not have, and `document.invalid_value`
/// for a value that does not fit its field, each as a field error.
pub fn patch_from_json(
    def: &DocTypeDef,
    patch: &Json,
    mut new_row_id: impl FnMut() -> RecordId,
) -> AppResult<DocumentPatch> {
    let object = patch
        .as_object()
        .ok_or_else(|| bad_patch("", "the patch must be an object"))?;
    let mut out = DocumentPatch::default();
    for (name, value) in object {
        if matches!(name.as_str(), "id" | "version" | "_state") {
            continue;
        }
        let Some(field) = def.field(name) else {
            return Err(AppError::field(
                FieldPath::field(name),
                "document.unknown_field",
                format!("The DocType `{}` has no field `{name}`.", def.name),
            ));
        };
        match &field.kind {
            FieldKind::Table { .. } | FieldKind::TableMultiselect { .. } => {
                let rows = value
                    .as_array()
                    .ok_or_else(|| bad_patch(name, "expected an array of rows"))?;
                let mut parsed = Vec::with_capacity(rows.len());
                for row in rows {
                    parsed.push(row_patch(name, row, &mut new_row_id)?);
                }
                let table = Name::new(name).map_err(|_| bad_patch(name, "not a storage name"))?;
                out.tables.insert(table, parsed);
            }
            kind => {
                let converted = json_to_value(kind, value).map_err(|why| bad_patch(name, &why))?;
                let field_name =
                    Name::new(name).map_err(|_| bad_patch(name, "not a storage name"))?;
                out.fields.insert(field_name, converted);
            }
        }
    }
    Ok(out)
}

/// A new document from the JSON a script passed to `host.insert`.
///
/// # Errors
///
/// `document.unknown_field` and `document.invalid_value` as above.
pub fn document_from_json(def: &DocTypeDef, values: &Json, id: RecordId) -> AppResult<Document> {
    let mut doc = Document::new(def.name.clone(), id);
    let patch = patch_from_json(def, values, RecordId::new_v7)?;
    for (name, value) in patch.fields {
        doc.set(name, value);
    }
    for (table, rows) in patch.tables {
        let target = doc.rows_mut(table);
        for row in rows {
            target.push(spot_app_api::ChildRow {
                id: row.id.unwrap_or_else(RecordId::new_v7),
                fields: row.fields,
            });
        }
    }
    Ok(doc)
}

fn row_patch(
    table: &str,
    row: &Json,
    new_row_id: &mut impl FnMut() -> RecordId,
) -> AppResult<RowPatch> {
    let object = row
        .as_object()
        .ok_or_else(|| bad_patch(table, "a row must be an object"))?;
    let mut patch = RowPatch::default();
    for (name, value) in object {
        if name == "id" {
            patch.id = value.as_str().and_then(parse_record_id);
            continue;
        }
        // The child field kinds are not in the parent's definition; a row value keeps the shape
        // the JSON gives it, and the engine validates the child document on write.
        let field = Name::new(name).map_err(|_| bad_patch(name, "not a storage name"))?;
        patch.fields.insert(field, json_row_value(value));
    }
    if patch.id.is_none() {
        patch.id = Some(new_row_id());
    }
    Ok(patch)
}

/// A row value: a scalar keeps its JSON shape as a storage value without a child definition.
fn json_row_value(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Text(n.to_string())
            }
        }
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Json(other.to_string()),
    }
}

fn bad_patch(field: &str, why: &str) -> AppError {
    let path = FieldPath::parse(field).unwrap_or_else(|| FieldPath::field(field));
    AppError::field(path, "document.invalid_value", why.to_owned())
}

fn decimal_text(value: &Json) -> Result<String, String> {
    match value {
        Json::String(s) => Ok(s.clone()),
        Json::Number(n) => Ok(n.to_string()),
        _ => Err("expected a decimal string".to_owned()),
    }
}

fn format_date(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

fn format_time(time: Time) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        time.hour(),
        time.minute(),
        time.second()
    )
}

fn format_datetime(dt: OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| dt.unix_timestamp().to_string())
}

fn parse_date(text: &str) -> Option<Date> {
    let mut parts = text.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()
}

fn parse_time(text: &str) -> Option<Time> {
    let mut parts = text.split(':');
    let hour: u8 = parts.next()?.parse().ok()?;
    let minute: u8 = parts.next()?.parse().ok()?;
    let second: u8 = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Time::from_hms(hour, minute, second).ok()
}

fn parse_datetime(value: &Json) -> Option<OffsetDateTime> {
    if let Some(seconds) = value.as_i64() {
        return OffsetDateTime::from_unix_timestamp(seconds).ok();
    }
    let text = value.as_str()?;
    OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .or_else(|| OffsetDateTime::from_unix_timestamp(text.parse().ok()?).ok())
}

/// Parses a UUID string into a record id. Only version 7 ids are accepted (ADR-33).
pub fn parse_record_id(text: &str) -> Option<RecordId> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    RecordId::from_bytes(bytes).ok()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_app_api::DocState;
    use spot_storage_contract::Version;

    fn doc() -> Document {
        let mut doc = Document::loaded(
            Name::new("note").unwrap(),
            RecordId::new_v7(),
            Version::FIRST,
            Some(DocState::Draft),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        );
        doc.set(Name::new("title").unwrap(), Value::Text("hi".into()));
        doc.set(
            Name::new("total").unwrap(),
            Value::Decimal(spot_money::rounding::parse_decimal("12.50").unwrap()),
        );
        doc
    }

    #[test]
    fn a_decimal_travels_as_its_exact_string() {
        let json = document_json(&doc(), None);
        assert_eq!(json["total"], json!("12.50"));
        assert_eq!(json["title"], json!("hi"));
        assert_eq!(json["_state"], json!("draft"));
    }

    #[test]
    fn projection_keeps_only_named_fields() {
        let json = document_json(&doc(), Some(&["title".to_owned()]));
        assert!(json.get("total").is_none());
        assert_eq!(json["title"], json!("hi"));
    }

    #[test]
    fn a_decimal_parses_from_a_string_or_a_number() {
        let kind = FieldKind::Decimal {
            scale: 2,
            precision: 12,
            format: spot_app_api::meta::DecimalFormat::Plain,
        };
        assert_eq!(
            json_to_value(&kind, &json!("3.14")).unwrap(),
            Value::Decimal(spot_money::rounding::parse_decimal("3.14").unwrap())
        );
        assert_eq!(
            json_to_value(&kind, &json!(3)).unwrap(),
            Value::Decimal(spot_money::rounding::parse_decimal("3").unwrap())
        );
    }

    #[test]
    fn a_record_id_round_trips() {
        let id = RecordId::new_v7();
        assert_eq!(parse_record_id(&id.to_string()), Some(id));
    }

    #[test]
    fn a_link_value_parses() {
        let kind = FieldKind::Link {
            doctype: Name::new("user").unwrap(),
        };
        let id = RecordId::new_v7();
        assert_eq!(
            json_to_value(&kind, &json!(id.to_string())).unwrap(),
            Value::Id(id)
        );
        assert!(json_to_value(&kind, &json!("not-an-id")).is_err());
    }
}
