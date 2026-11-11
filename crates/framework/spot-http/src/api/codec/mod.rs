//! The codec translates between the HTTP wire's JSON and storage values. Field scalars
//! and the per-field-type rules (decode/encode, strict number and string parsing, id
//! parsing) live here; whole documents, child-table rows, `DocumentPatchApi` and
//! `MetaLookup` live in [`rows`].
//!
//! Write-body rules (ADR-055): every field must exist in the meta, `_`-keys and
//! `version` are rejected, and every write string is validated through
//! [`valid_write_key`]/[`Name::new`].

use base64::Engine as _;
use rust_decimal::Decimal;
use serde_json::Value as Json;
use spot_storage_contract::{Name, RecordId, Value};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, OffsetDateTime, Time, UtcOffset};
use uuid::Uuid;

use super::meta::{DocTypeMeta, FieldType, MetaField};
use super::registry::Code;
use super::service::ApiError;

/// A field's value inside an [`ApiDocument`]: a plain storage value, or a child table's rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldValue {
    /// A scalar storage value.
    Scalar(Value),
    /// A child table's rows, in `idx` order.
    Rows(Vec<ChildRowDoc>),
}

/// Documents and child-table rows: `ApiDocument`, `DocumentPatchApi`, `MetaLookup`, and
/// the row-level decode/encode functions.
mod rows;
pub use rows::{
    ApiDocument, ChildRowDoc, CurrencyInfo, DocumentPatchApi, MetaLookup, decode_row,
    encode_document, encode_row,
};
/// Storage name validated by `valid_write_key` upstream; re-checked here, never assumed.
fn checked_name(key: &str) -> Result<Name, ApiError> {
    Name::new(key).map_err(|_| {
        ApiError::new(
            Code::ApiInvalidRequest,
            format!("`{key}` is not a valid field name."),
        )
    })
}
/// How a request body is interpreted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// `POST` create: a whole document; `id` optional (a client may choose one, ADR-055).
    Create,
    /// `PATCH` update: a merge patch; absent fields keep their values.
    Patch,
}

/// Reject request bodies that carry `version` or `_`-keys (server metadata) before any other
/// decoding: `api.invalid_request`. Returns the offending key.
fn reserved_key(key: &str) -> bool {
    key == "version" || key.starts_with('_')
}

/// Whether a string is the contract's `FieldPath`: a field name, or `table[idx].field` with
/// idx from 1, both parts storage-style identifiers.
pub fn is_field_path(path: &str) -> bool {
    fn ident(s: &str) -> bool {
        let mut bytes = s.bytes();
        bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }
    match path.split_once('[') {
        None => ident(path),
        Some((table, rest)) => {
            let Some((idx, field)) = rest.split_once("].") else {
                return false;
            };
            ident(table)
                && ident(field)
                && !idx.starts_with('0')
                && !idx.is_empty()
                && idx.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

/// Whether a key matches the contract's `DocumentWrite.propertyNames` pattern: a storage name
/// (`[a-z][a-z0-9_]{0,62}`) that is not `version`.
fn valid_write_key(key: &str) -> bool {
    key != "version" && Name::new(key).is_ok()
}
/// Decode a request body (`DocumentWrite`) into a document (create) or a patch (update).
///
/// `id` in a patch body must equal `path_id`; a mismatch is `api.invalid_request`.
/// Every offending unknown field is collected into one `document.unknown_field` error.
pub fn decode_body(
    body: &Json,
    meta: &DocTypeMeta,
    mode: WriteMode,
    path_id: Option<RecordId>,
    meta_of: MetaLookup,
) -> Result<ApiDocument, ApiError> {
    let obj = match body {
        Json::Object(map) => map,
        _ => {
            return Err(ApiError::new(
                Code::ApiInvalidRequest,
                "The request body must be a JSON object.",
            ));
        }
    };

    let mut doc = ApiDocument::empty();
    let mut unknown: Vec<String> = Vec::new();
    let mut errors: Vec<ApiError> = Vec::new();

    for (key, value) in obj {
        if reserved_key(key) {
            return Err(ApiError::new(
                Code::ApiInvalidRequest,
                format!("The key `{key}` is server metadata and cannot be sent in a request."),
            ));
        }
        if !valid_write_key(key) {
            return Err(ApiError::new(
                Code::ApiInvalidRequest,
                format!("`{key}` is not a valid field name."),
            ));
        }
        if key == "id" {
            // The storage `id` system key, not a field.
            let id = decode_id_value(value, "id", &mut errors);
            if mode == WriteMode::Patch
                && let (Some(sent), Some(path)) = (id, path_id)
                && sent != path
            {
                return Err(ApiError::new(
                    Code::ApiInvalidRequest,
                    "The body `id` does not match the document in the path.",
                ));
            }
            doc.id = id;
            continue;
        }
        match meta.field(key) {
            None => unknown.push(key.clone()),
            Some(field) => match decode_field_value(value, field, meta_of) {
                Ok(fv) => {
                    doc.fields.insert(checked_name(key)?, fv);
                }
                Err(e) => errors.push(e),
            },
        }
    }

    if !unknown.is_empty() {
        // `document.unknown_field` with every offending field listed.
        let message = format!(
            "The DocType `{}` has no such field{}.",
            meta.doctype.as_str(),
            if unknown.len() == 1 { "" } else { "s" }
        );
        let mut err = ApiError::new(Code::DocumentUnknownField, message);
        for field in &unknown {
            err.fields.push(super::service::FieldIssue::new(
                field.clone(),
                "document.unknown_field",
                "The DocType has no such field.",
            ));
        }
        return Err(err);
    }
    if let Some(first) = errors.into_iter().next() {
        return Err(first);
    }
    Ok(doc)
}

fn decode_id_value(value: &Json, field: &str, errors: &mut Vec<ApiError>) -> Option<RecordId> {
    match value {
        Json::Null => None,
        Json::String(s) => match parse_record_id(s) {
            Some(id) => Some(id),
            None => {
                errors.push(invalid_value(field, "a canonical lower-case UUIDv7"));
                None
            }
        },
        _ => {
            errors.push(invalid_value(field, "a document id string"));
            None
        }
    }
}

/// Decode one field value per its meta type; child tables recurse with the child's meta.
fn decode_field_value(
    value: &Json,
    field: &MetaField,
    meta_of: MetaLookup,
) -> Result<FieldValue, ApiError> {
    if field.field_type == FieldType::Table {
        let child = field
            .child_doctype
            .as_ref()
            .ok_or_else(|| ApiError::from_code(Code::HttpInternal))?;
        let child_meta = meta_of.get(child.as_str())?;
        let Json::Array(rows) = value else {
            return Err(invalid_value(
                field.name.as_str(),
                "an array of row objects",
            ));
        };
        let mut out = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            out.push(decode_row(row, field.name.as_str(), i + 1, &child_meta)?);
        }
        return Ok(FieldValue::Rows(out));
    }
    Ok(FieldValue::Scalar(decode_scalar(
        value,
        field,
        field.name.as_str(),
    )?))
}
/// Decode a JSON value into a storage `Value` for one field, per its type.
pub fn decode_scalar(value: &Json, field: &MetaField, path: &str) -> Result<Value, ApiError> {
    if matches!(value, Json::Null) {
        return Ok(Value::Null);
    }
    match &field.field_type {
        FieldType::Bool => match value {
            Json::Bool(b) => Ok(Value::Bool(*b)),
            _ => Err(invalid_value(path, "true or false")),
        },
        FieldType::Int => match value.as_i64() {
            Some(i) => Ok(Value::Int(i)),
            None => Err(invalid_value(path, "a 64-bit integer")),
        },
        FieldType::Decimal { scale } => {
            let Json::String(s) = value else {
                return Err(invalid_value(path, "a decimal string"));
            };
            parse_decimal_strict(s)
                .ok_or_else(|| invalid_value(path, "a decimal string"))
                .and_then(|d| {
                    if d.scale() > u32::from(*scale) {
                        Err(ApiError::new(
                            Code::DocumentInvalidValue,
                            format!(
                                "`{path}` has more fractional digits than its scale {scale} allows."
                            ),
                        ))
                    } else {
                        Ok(Value::Decimal(d))
                    }
                })
        }
        FieldType::Text => match value {
            Json::String(s) => Ok(Value::Text(s.clone())),
            _ => Err(invalid_value(path, "a string")),
        },
        FieldType::Date => match value {
            Json::String(s) => parse_date(s)
                .map(Value::Date)
                .ok_or_else(|| invalid_value(path, "a date `YYYY-MM-DD`")),
            _ => Err(invalid_value(path, "a date string")),
        },
        FieldType::Time => match value {
            Json::String(s) => parse_time(s)
                .map(Value::Time)
                .ok_or_else(|| invalid_value(path, "a time `HH:MM:SS`")),
            _ => Err(invalid_value(path, "a time string")),
        },
        FieldType::DateTime => match value {
            Json::String(s) => parse_datetime(s)
                .map(Value::DateTime)
                .ok_or_else(|| invalid_value(path, "an RFC 3339 datetime")),
            _ => Err(invalid_value(path, "a datetime string")),
        },
        FieldType::Link => match value {
            Json::String(s) => parse_record_id(s)
                .map(Value::Id)
                .ok_or_else(|| invalid_value(path, "a canonical UUIDv7 link")),
            _ => Err(invalid_value(path, "a link string")),
        },
        FieldType::Json => {
            // Embedded JSON, not stringified; stored as canonical text.
            let text = serde_json::to_string(value).map_err(|_| {
                ApiError::new(
                    Code::DocumentInvalidValue,
                    format!("`{path}` is not encodable JSON."),
                )
            })?;
            Ok(Value::Json(text))
        }
        FieldType::Bytes => match value {
            Json::String(s) => base64::engine::general_purpose::STANDARD
                .decode(s)
                .map(Value::Bytes)
                .map_err(|_| invalid_value(path, "standard base64 with padding")),
            _ => Err(invalid_value(path, "a base64 string")),
        },
        FieldType::Table => Err(invalid_value(path, "an array of row objects (child table)")),
        // An `attach` field encodes as a file-record id (WP-PLT-10); v0.1 write bodies carry
        // no attachment values, so only the string form arrives here.
        FieldType::Attach => match value {
            Json::String(s) => parse_record_id(s)
                .map(Value::Id)
                .ok_or_else(|| invalid_value(path, "a file record id string")),
            _ => Err(invalid_value(path, "a file record id string")),
        },
    }
}

/// A decimal string per the Values table: `^-?[0-9]+(\.[0-9]+)?$`. No exponent, no `+`.
pub fn parse_decimal_strict(s: &str) -> Option<Decimal> {
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", s),
    };
    let mut parts = digits.split('.');
    let int_part = parts.next().unwrap_or("");
    let frac_part = parts.next();
    if parts.next().is_some() {
        return None; // more than one dot
    }
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(frac) = frac_part
        && (frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    // rust_decimal's from_str accepts more syntax than we allow; re-check via the canonical
    // string form so scale and sign survive the round trip. Negative zero is accepted and
    // normalized (the type has no sign for zero; the contract's pattern allows the syntax).
    let parsed = Decimal::from_str_exact(&format!("{sign}{digits}")).ok()?;
    let canonical = format!("{sign}{digits}");
    if parsed.to_string() != canonical && !(sign == "-" && parsed.is_zero()) {
        return None;
    }
    Some(parsed)
}

/// `YYYY-MM-DD`, well-formed (calendar-checked).
pub fn parse_date(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if y.len() != 4 || m.len() == 1 || d.len() == 1 {
        return None;
    }
    if !y.bytes().all(|b| b.is_ascii_digit())
        || !m.bytes().all(|b| b.is_ascii_digit())
        || !d.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let year: i32 = y.parse().ok()?;
    let month: u8 = m.parse().ok()?;
    let day: u8 = d.parse().ok()?;
    Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

/// `HH:MM:SS` with optional `.ffffff`.
pub fn parse_time(s: &str) -> Option<Time> {
    let (clock, fraction) = match s.split_once('.') {
        Some((c, f)) => {
            if f.is_empty() || f.len() > 6 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (c, f)
        }
        None => (s, ""),
    };
    let mut parts = clock.split(':');
    let (Some(h), Some(m), Some(sec), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if [h, m, sec]
        .iter()
        .any(|p| p.len() != 2 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let hour: u8 = h.parse().ok()?;
    let minute: u8 = m.parse().ok()?;
    let second: u8 = sec.parse().ok()?;
    let micro: u32 = if fraction.is_empty() {
        0
    } else {
        let padded = format!("{fraction:0<6}");
        padded.parse().ok()?
    };
    Time::from_hms_micro(hour, minute, second, micro).ok()
}

/// RFC 3339 with any offset, converted to UTC.
pub fn parse_datetime(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|dt| dt.to_offset(UtcOffset::UTC))
}

/// A canonical lower-case hyphenated UUIDv7.
pub fn parse_record_id(s: &str) -> Option<RecordId> {
    let u = Uuid::parse_str(s).ok()?;
    // `parse_str` accepts urn/braced/hyphenless forms; the contract fixes the canonical one.
    if u.to_string() != s || u.get_version_num() != 7 {
        return None;
    }
    RecordId::from_bytes(*u.as_bytes()).ok()
}

fn invalid_value(path: &str, wanted: &str) -> ApiError {
    let mut err = ApiError::new(
        Code::DocumentInvalidValue,
        format!("The value for `{path}` does not fit its field type: expected {wanted}."),
    );
    err.fields.push(super::service::FieldIssue::new(
        path.to_owned(),
        "document.invalid_value",
        "A value does not fit its field type.",
    ));
    err
}

/// Encode one scalar `Value` for a field per the Values table (responses always use UTC for
/// instants).
pub fn encode_scalar(value: &Value, field: &MetaField) -> Result<Json, ApiError> {
    let encoded = match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Date(d) => Json::String(
            d.format(&format_description!("[year]-[month]-[day]"))
                .map_err(internal)?,
        ),
        Value::Time(t) => Json::String(encode_time(t)?),
        Value::DateTime(dt) => Json::String(encode_datetime(dt)),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Json(text) => {
            serde_json::from_str(text).map_err(|_| ApiError::from_code(Code::HttpInternal))?
        }
        Value::Bytes(b) => Json::String(base64::engine::general_purpose::STANDARD.encode(b)),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); refuse rather than guess.
        Value::GeoPoint(_) | Value::Range(_) => {
            return Err(ApiError::from_code(Code::HttpInternal));
        }
    };
    // A stored value that contradicts its declared type is a server bug, not a client error.
    match &field.field_type {
        FieldType::Table => return Err(ApiError::from_code(Code::HttpInternal)),
        ft if !value.is_null() && !value_fits(ft, value) => {
            return Err(ApiError::from_code(Code::HttpInternal));
        }
        _ => {}
    }
    Ok(encoded)
}

fn internal(_e: impl std::fmt::Display) -> ApiError {
    ApiError::from_code(Code::HttpInternal)
}

fn value_fits(t: &FieldType, v: &Value) -> bool {
    matches!(
        (v, t),
        (Value::Bool(_), FieldType::Bool)
            | (Value::Int(_), FieldType::Int)
            | (Value::Decimal(_), FieldType::Decimal { .. })
            | (Value::Text(_), FieldType::Text)
            | (Value::Date(_), FieldType::Date)
            | (Value::Time(_), FieldType::Time)
            | (Value::DateTime(_), FieldType::DateTime)
            | (Value::Id(_), FieldType::Link)
            | (Value::Json(_), FieldType::Json)
            | (Value::Bytes(_), FieldType::Bytes)
            | (Value::Id(_), FieldType::Attach)
    )
}

/// Encode an instant as RFC 3339 UTC ending in `Z`, fractional digits only when nonzero.
pub fn encode_datetime(dt: &OffsetDateTime) -> String {
    let utc = dt.to_offset(UtcOffset::UTC);
    encode_instant(&utc)
}

fn encode_instant(utc: &OffsetDateTime) -> String {
    let date = utc.date();
    let tod = utc.time();
    let mut text = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        date.year(),
        u8::from(date.month()),
        date.day(),
        tod.hour(),
        tod.minute(),
        tod.second()
    );
    let micro = tod.microsecond();
    if micro != 0 {
        let frac = format!(".{micro:06}");
        text.push_str(frac.trim_end_matches('0'));
    }
    text.push('Z');
    text
}

/// `HH:MM:SS` with `.ffffff` (microseconds) only when there is a sub-second part.
pub fn encode_time(t: &Time) -> Result<String, ApiError> {
    let fmt = format_description!("[hour]:[minute]:[second]");
    let mut text = t.format(&fmt).map_err(internal)?;
    let micro = t.microsecond();
    if micro != 0 {
        let frac = format!(".{micro:06}");
        text.push_str(frac.trim_end_matches('0'));
    }
    Ok(text)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::meta::FieldHints;
    use crate::{Must, MustErr};
    use std::collections::BTreeMap;

    fn name(s: &str) -> Name {
        Name::new(s).must()
    }

    fn scalar_field(n: &str, t: FieldType) -> MetaField {
        MetaField {
            name: name(n),
            label: n.to_owned(),
            field_type: t,
            required: false,
            read_only: false,
            scale: None,
            precision: None,
            link_to: None,
            link_type_field: None,
            child_doctype: None,
            link_field: None,
            select: None,
            max_chars: None,
            default: None,
            format: None,
            format_params: BTreeMap::new(),
            lookup: None,
            hints: FieldHints::default(),
            has_hints: false,
            multiple: false,
        }
    }

    fn meta_with(fields: Vec<MetaField>) -> DocTypeMeta {
        DocTypeMeta {
            doctype: name("sales_invoice"),
            label: "Sales Invoice".to_owned(),
            version: 3,
            is_posted: true,
            fields,
        }
    }

    #[test]
    fn decimal_strict_syntax() {
        assert!(parse_decimal_strict("10.50").is_some());
        assert!(parse_decimal_strict("-7").is_some());
        assert!(parse_decimal_strict("1e5").is_none(), "exponent rejected");
        assert!(
            parse_decimal_strict("+5").is_none(),
            "leading plus rejected"
        );
        assert!(parse_decimal_strict("1.2.3").is_none());
        assert!(parse_decimal_strict("5.").is_none());
        assert!(parse_decimal_strict(".5").is_none());
        assert!(parse_decimal_strict("").is_none());
        assert!(parse_decimal_strict("-0").is_some());
    }

    #[test]
    fn decimal_scale_is_kept() {
        let d = parse_decimal_strict("10.50").must();
        assert_eq!(d.to_string(), "10.50");
    }

    #[test]
    fn json_number_for_decimal_rejected() {
        let m = meta_with(vec![scalar_field(
            "grand_total",
            FieldType::Decimal { scale: 2 },
        )]);
        let body = serde_json::json!({ "grand_total": 12.5 });
        let err = decode_body(&body, &m, WriteMode::Create, None, MetaLookup::none()).must_err();
        assert_eq!(
            err.code,
            super::super::service::ErrorCode::Registry(Code::DocumentInvalidValue)
        );
    }

    #[test]
    fn datetime_offset_converts_to_utc() {
        let dt = parse_datetime("2026-09-29T15:30:00+05:30").must();
        assert_eq!(encode_datetime(&dt), "2026-09-29T10:00:00Z");
    }

    #[test]
    fn record_id_requires_canonical_v7() {
        assert!(parse_record_id("0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f").is_some());
        assert!(
            parse_record_id("0192F3C4-5b6a-7c8d-9e0f-1a2b3c4d5e6f").is_none(),
            "upper case is not canonical"
        );
        assert!(
            parse_record_id("0192f3c45b6a7c8d9e0f1a2b3c4d5e6f").is_none(),
            "no hyphens is not canonical"
        );
        assert!(
            parse_record_id("00000000-0000-4000-8000-000000000000").is_none(),
            "v4 is not a v7"
        );
    }

    #[test]
    fn encode_time_trims_fraction() {
        let t = parse_time("12:30:00").must();
        assert_eq!(encode_time(&t).must(), "12:30:00");
        let t = parse_time("00:00:00.500000").must();
        assert_eq!(encode_time(&t).must(), "00:00:00.5");
    }

    #[test]
    fn reserved_keys_rejected() {
        let m = meta_with(vec![]);
        for key in ["version", "_state", "_x"] {
            let body = Json::Object([(key.to_owned(), Json::Null)].into_iter().collect());
            let err =
                decode_body(&body, &m, WriteMode::Create, None, MetaLookup::none()).must_err();
            assert_eq!(
                err.code,
                super::super::service::ErrorCode::Registry(Code::ApiInvalidRequest),
                "key `{key}` must be rejected as api.invalid_request"
            );
        }
    }

    #[test]
    fn unknown_fields_list_every_offender() {
        let m = meta_with(vec![]);
        let body = Json::Object(
            [
                ("ghost_a".to_owned(), Json::Bool(true)),
                ("ghost_b".to_owned(), Json::from(1)),
            ]
            .into_iter()
            .collect(),
        );
        let err = decode_body(&body, &m, WriteMode::Create, None, MetaLookup::none()).must_err();
        assert_eq!(
            err.code,
            super::super::service::ErrorCode::Registry(Code::DocumentUnknownField)
        );
        assert_eq!(err.fields.len(), 2, "every offending field is listed");
    }

    #[test]
    fn child_rows_decode_recursively_with_child_meta() {
        let child = meta_with(vec![
            scalar_field("qty", FieldType::Decimal { scale: 2 }),
            scalar_field("note", FieldType::Text),
        ]);
        let mut parent_meta = meta_with(vec![MetaField {
            name: name("items"),
            label: "Items".to_owned(),
            field_type: FieldType::Table,
            required: false,
            read_only: false,
            scale: None,
            precision: None,
            link_to: None,
            link_type_field: None,
            child_doctype: Some(name("sales_invoice_item")),
            link_field: None,
            select: None,
            max_chars: None,
            default: None,
            format: None,
            format_params: BTreeMap::new(),
            lookup: None,
            hints: FieldHints::default(),
            has_hints: false,
            multiple: false,
        }]);
        parent_meta.doctype = name("sales_invoice");
        let mut lookup_map = std::collections::BTreeMap::new();
        lookup_map.insert("sales_invoice_item".to_owned(), child);
        let body = serde_json::json!({
            "items": [
                {"qty": "2", "note": "first"},
                {"id": "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e70", "qty": "1.50"}
            ]
        });
        let doc = decode_body(
            &body,
            &parent_meta,
            WriteMode::Create,
            None,
            MetaLookup::over(&lookup_map),
        )
        .must();
        let FieldValue::Rows(rows) = doc.fields.get(&name("items")).must() else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, None);
        assert_eq!(
            rows[1].id,
            parse_record_id("0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e70")
        );
        assert_eq!(
            rows[1].fields.get(&name("qty")),
            Some(&Value::Decimal(parse_decimal_strict("1.50").must()))
        );
    }

    #[test]
    fn row_idx_key_is_rejected() {
        let child = meta_with(vec![]);
        let parent_meta = meta_with(vec![MetaField {
            name: name("items"),
            label: "Items".to_owned(),
            field_type: FieldType::Table,
            required: false,
            read_only: false,
            scale: None,
            precision: None,
            link_to: None,
            link_type_field: None,
            child_doctype: Some(name("sales_invoice_item")),
            link_field: None,
            select: None,
            max_chars: None,
            default: None,
            format: None,
            format_params: BTreeMap::new(),
            lookup: None,
            hints: FieldHints::default(),
            has_hints: false,
            multiple: false,
        }]);
        let mut lookup_map = std::collections::BTreeMap::new();
        lookup_map.insert("sales_invoice_item".to_owned(), child);
        let body = serde_json::json!({"items": [{"idx": 1}]});
        let err = decode_body(
            &body,
            &parent_meta,
            WriteMode::Create,
            None,
            MetaLookup::over(&lookup_map),
        )
        .must_err();
        assert_eq!(
            err.code,
            super::super::service::ErrorCode::Registry(Code::ApiInvalidRequest)
        );
    }

    #[test]
    fn decimal_scale_overflow_rejected_never_rounded() {
        let m = meta_with(vec![scalar_field("rate", FieldType::Decimal { scale: 2 })]);
        let body = serde_json::json!({"rate": "1.234"});
        let err = decode_body(&body, &m, WriteMode::Create, None, MetaLookup::none()).must_err();
        assert_eq!(
            err.code,
            super::super::service::ErrorCode::Registry(Code::DocumentInvalidValue)
        );
    }

    #[test]
    fn base64_round_trip_and_json_embedding() {
        let m = meta_with(vec![
            scalar_field("signature", FieldType::Bytes),
            scalar_field("payload", FieldType::Json),
        ]);
        let body = serde_json::json!({"signature": "aGk=", "payload": {"a": [1, 2]}});
        let doc = decode_body(&body, &m, WriteMode::Create, None, MetaLookup::none()).must();
        let encoded = encode_document(&doc, &m, None, MetaLookup::none()).must();
        assert_eq!(encoded["signature"], "aGk=");
        assert_eq!(encoded["payload"], serde_json::json!({"a": [1, 2]}));
        // bytes with wrong padding are rejected
        let bad = serde_json::json!({"signature": "aGk"});
        let err = decode_body(&bad, &m, WriteMode::Create, None, MetaLookup::none()).must_err();
        assert_eq!(
            err.code,
            super::super::service::ErrorCode::Registry(Code::DocumentInvalidValue)
        );
    }

    #[test]
    fn patch_body_splits_fields_and_tables() {
        let m = meta_with(vec![scalar_field("remarks", FieldType::Text)]);
        let body = serde_json::json!({"remarks": "after 5 pm"});
        let doc = decode_body(&body, &m, WriteMode::Patch, None, MetaLookup::none()).must();
        let patch = doc.into_patch();
        assert_eq!(
            patch.fields.get(&name("remarks")),
            Some(&Value::Text("after 5 pm".to_owned()))
        );
        assert!(patch.tables.is_empty());
    }
}
