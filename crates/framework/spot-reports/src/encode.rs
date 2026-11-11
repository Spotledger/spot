//! The contract's value encoding for a report row: a storage [`Value`] and a column's
//! [`FieldType`] to the JSON of `ReportResult.rows`, per the native API's Values table.
//!
//! Decimals are strings with their scale preserved (ADR-12: no floats, no rounding here);
//! an instant is RFC 3339 UTC ending in `Z`; bytes are base64. A value that contradicts its
//! declared type is a server bug and encodes as `null` rather than a wrong shape.

use base64::Engine as _;
use rust_decimal::Decimal;
use serde_json::Value as Json;
use spot_storage_contract::Value;

pub use spot_http::api::meta::FieldType;

/// The contract type of a storage field type.
#[must_use]
pub fn contract_type(ty: &spot_storage_contract::schema::FieldType) -> FieldType {
    use spot_storage_contract::schema::FieldType as S;
    match ty {
        S::Bool => FieldType::Bool,
        S::Int => FieldType::Int,
        S::Decimal { scale, .. } => FieldType::Decimal { scale: *scale },
        S::Text { .. } => FieldType::Text,
        S::Date => FieldType::Date,
        S::Time => FieldType::Time,
        S::DateTime => FieldType::DateTime,
        S::Link { .. } => FieldType::Link,
        S::Json => FieldType::Json,
        S::Bytes => FieldType::Bytes,
        // ADR-087: the native API has no geo or range type yet (WP-MET-05.T1); such a column
        // reads as JSON, and `encode_value` encodes a value that does not fit as null.
        S::Geo { .. } | S::Range { .. } => FieldType::Json,
    }
}

/// A decimal as its exact string, scale preserved.
#[must_use]
pub fn decimal_string(d: &Decimal) -> String {
    d.to_string()
}

/// Encodes one value for a column of `ty`. A value that does not fit the type encodes as
/// `null`: the mismatch is a server bug the caller must not see as a wrong shape.
#[must_use]
pub fn encode_value(value: &Value, ty: &FieldType) -> Json {
    match (value, ty) {
        (Value::Null, _) => Json::Null,
        (Value::Bool(b), FieldType::Bool) => Json::Bool(*b),
        (Value::Int(i), FieldType::Int) => Json::from(*i),
        (Value::Decimal(d), FieldType::Decimal { .. }) => Json::String(decimal_string(d)),
        (Value::Text(t), FieldType::Text) => Json::String(t.clone()),
        (Value::Date(d), FieldType::Date) => Json::String(date_string(d)),
        (Value::Time(t), FieldType::Time) => Json::String(time_string(t)),
        (Value::DateTime(dt), FieldType::DateTime) => Json::String(datetime_string(dt)),
        (Value::Id(id), FieldType::Link) => Json::String(id.to_string()),
        (Value::Json(text), FieldType::Json) => serde_json::from_str(text).unwrap_or(Json::Null),
        (Value::Bytes(b), FieldType::Bytes) => {
            Json::String(base64::engine::general_purpose::STANDARD.encode(b))
        }
        _ => Json::Null,
    }
}

/// `YYYY-MM-DD`.
#[must_use]
pub fn date_string(d: &time::Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

/// `HH:MM:SS` with fractional seconds only when nonzero.
#[must_use]
pub fn time_string(t: &time::Time) -> String {
    let (h, m, s, ns) = t.as_hms_nano();
    if ns == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        let frac = format!("{ns:09}");
        let frac = frac.trim_end_matches('0');
        format!("{h:02}:{m:02}:{s:02}.{frac}")
    }
}

/// RFC 3339 UTC ending in `Z`, fractional digits only when nonzero.
#[must_use]
pub fn datetime_string(dt: &time::OffsetDateTime) -> String {
    let utc = dt.to_offset(time::UtcOffset::UTC);
    let date = date_string(&utc.date());
    let time = time_string(&utc.time());
    format!("{date}T{time}Z")
}

/// The masked form of a value: text is masked, anything else becomes null (the same choice the
/// document read path makes), so a mask never leaks a value's shape.
#[must_use]
pub fn masked_value(value: &Value, format: &spot_permissions::MaskFormat) -> Value {
    match value {
        Value::Text(text) => Value::Text(spot_permissions::mask(text, format)),
        _ => Value::Null,
    }
}
