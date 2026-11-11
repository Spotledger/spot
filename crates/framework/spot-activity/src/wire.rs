//! Serde for the ADR-055 value encoding, ids, names, instants and document versions: the
//! helpers every `#[serde(with …)]` attribute in this crate names, one **module** per field
//! shape (serde wants a module with `serialize` and `deserialize`).
//!
//! The storage contract's [`Value`] carries no serde support (it is the storage-side model), so
//! this module gives every activity record a stable wire form: decimals as JSON strings with
//! their scale kept, timestamps RFC 3339 in UTC, dates `YYYY-MM-DD`, times `HH:MM:SS[.f]`, ids
//! as canonical lower-case hyphenated UUIDv7, JSON embedded (never stringified) and bytes
//! base64. An event payload and an API response never disagree about how a value is written
//! (ADR-056): the same encoding writes both.
//!
//! The encoding is type-less: a JSON string's meaning is its shape, exactly as the native API
//! writes values. Consumers that must tell a text `2026` and the decimal `2026` apart use the
//! field's type; these history records do not carry types, so this module follows the same
//! shape rules without guessing.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, Value};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

/// The ADR-055 JSON-to-value conversion, shared with the row snapshots.
#[allow(clippy::too_many_lines)]
pub(crate) fn from_json(v: serde_json::Value) -> Result<Value, String> {
    match v {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(b)),
        serde_json::Value::Number(n) => n.as_i64().map(Value::Int).ok_or_else(|| {
            "a decimal must be a JSON string, never a JSON number (ADR-055)".to_owned()
        }),
        serde_json::Value::String(text) => parse_text_value(&text),
        // Embedded JSON keeps its parsed form, re-serialised canonically.
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            Ok(Value::Json(serde_json::to_string(&v).unwrap_or_default()))
        }
    }
}

/// Parses a JSON string by shape: an id, a decimal, a timestamp, a date, a time, else text.
fn parse_text_value(text: &str) -> Result<Value, String> {
    let bytes = text.as_bytes();
    // An id: canonical lower-case hyphenated UUIDv7.
    if uuid_shape(bytes) {
        return record_id::parse(text).map(Value::Id);
    }
    // A decimal. (`1e2`-style input stays text: the API never sends it.)
    if !bytes.iter().any(|b| *b == b'e' || *b == b'E')
        && let Ok(dec) = text.parse::<spot_app_api::prelude::Decimal>()
    {
        return Ok(Value::Decimal(dec));
    }
    // A timestamp: RFC 3339 in UTC on the wire (`...Z`); an instant written with an offset is
    // accepted too and normalised to UTC. Anything else falls through to the later shapes.
    if text.len() > 10
        && text.as_bytes()[10] == b'T'
        && let Ok(parsed) = OffsetDateTime::parse(text, &Rfc3339)
        && let Some(utc) = parsed.checked_to_offset(time::UtcOffset::UTC)
    {
        return Ok(Value::DateTime(utc));
    }
    // A date.
    if let Ok(d) = time::Date::parse(text, &format_description!("[year]-[month]-[day]")) {
        return Ok(Value::Date(d));
    }
    // A time of day, with or without fractions.
    if let Ok(t) = time::Time::parse(
        text,
        &format_description!("[hour]:[minute]:[second][optional [.[subsecond digits:1+]]]"),
    ) {
        return Ok(Value::Time(t));
    }
    // Bytes on the wire are base64 (ADR-055); a type-less reader takes only the unmistakable
    // padded form — see `padded_base64`.
    if let Some(data) = padded_base64(text) {
        return Ok(Value::Bytes(data));
    }
    Ok(Value::Text(text.to_owned()))
}

/// Decodes base64 only when the string is unmistakably base64: RFC 4648 standard alphabet,
/// padding required, canonical. Length a multiple of four, one or two `=` and only at the
/// very end, the bits the padding stands for zero. Unpadded base64 is **not** decoded: shape
/// cannot tell it from text (`YWJj` is four letters of text as easily as three bytes, and the
/// empty string is `Text("")` per ADR-055), so a caller reading a `Bytes`-typed field decodes
/// it itself with the field's type; these records' `Text`-shaped fields never carry `=`.
fn padded_base64(text: &str) -> Option<Vec<u8>> {
    fn value(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some(u32::from(b - b'A')),
            b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    if !(text.len() >= 4 && text.len().is_multiple_of(4)) {
        return None;
    }
    let pad = bytes.iter().rev().take_while(|b| **b == b'=').count();
    if !(1..=2).contains(&pad) {
        return None;
    }
    let body = &bytes[..bytes.len() - pad];
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
    for (qi, chunk) in body.chunks(4).enumerate() {
        let is_last = qi + 1 == body.len().div_ceil(4);
        let mut word: u32 = 0;
        for (i, b) in chunk.iter().enumerate() {
            word |= value(*b)? << (18 - 6 * i);
        }
        // Canonical padding: for a two-char quantum the second character's low four bits are
        // the discarded ones; for a three-char quantum its low two.
        if is_last
            && match chunk.len() {
                2 => word & 0xF != 0,
                3 => word & 0x3 != 0,
                _ => false,
            }
        {
            return None;
        }
        out.extend_from_slice(
            &[(word >> 16) as u8, (word >> 8) as u8, word as u8][..chunk.len() - 1],
        );
    }
    Some(out)
}

/// The 36-char canonical hyphenated UUID shape.
fn uuid_shape(bytes: &[u8]) -> bool {
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// The six-digit subsecond form, trimmed to its significant digits.
fn trim_time(text: &str) -> &str {
    let trimmed = text.trim_end_matches('0');
    trimmed.strip_suffix('.').unwrap_or(trimmed)
}

/// A [`Value`] in the value encoding, as JSON (for building row snapshots).
pub(crate) fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::json!(i),
        Value::Decimal(d) => serde_json::json!(d.to_string()),
        Value::Text(t) => serde_json::json!(t),
        Value::Date(d) => serde_json::json!(d.to_string()),
        Value::Time(t) => {
            let text = t
                .format(&format_description!(
                    "[hour]:[minute]:[second].[subsecond digits:6]"
                ))
                .unwrap_or_default();
            serde_json::json!(trim_time(&text))
        }
        Value::DateTime(t) => serde_json::json!(t.format(&Rfc3339).unwrap_or_default()),
        Value::Id(id) => serde_json::json!(id.to_string()),
        Value::Json(j) => serde_json::from_str(j).unwrap_or(serde_json::Value::Null),
        Value::Bytes(b) => serde_json::json!(base64_encode(b)),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug form until
        // the wire encoding defines one.
        Value::GeoPoint(_) | Value::Range(_) => serde_json::json!(format!("{v:?}")),
    }
}

/// Base64 (RFC 4648, standard alphabet, padded) of the bytes value on the wire.
#[must_use]
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let word = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(word >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(word >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(word >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[word as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Base64 decode, for the bytes value read back in tests (standard alphabet, padding required).
#[cfg(test)]
#[allow(dead_code)]
fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    fn value(b: u8) -> Result<u32, String> {
        match b {
            b'A'..=b'Z' => Ok(u32::from(b - b'A')),
            b'a'..=b'z' => Ok(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(b - b'0') + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("not base64".to_owned()),
        }
    }
    let clean: Vec<u8> = text.trim_end_matches('=').bytes().collect();
    if clean.len() % 4 == 1 {
        return Err("a base64 string has an impossible length".to_owned());
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut word = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            word |= value(*b)? << (18 - 6 * i);
        }
        let bytes = [(word >> 16) as u8, (word >> 8) as u8, word as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Ok(out)
}

/// A [`Value`] in the value encoding, as a `#[serde(with)]` module.
pub mod value {
    use super::*;

    /// The value encoding.
    pub fn serialize<S: Serializer>(v: &Value, s: S) -> Result<S::Ok, S::Error> {
        value_to_json(v).serialize(s)
    }

    /// The value encoding.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        from_json(v).map_err(serde::de::Error::custom)
    }
}

/// An optional [`Value`], as the value encoding or absent.
pub mod opt_value {
    use super::*;

    /// The value encoding, or absent.
    pub fn serialize<S: Serializer>(v: &Option<Value>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => value::serialize(v, s),
            None => s.serialize_none(),
        }
    }

    /// The value encoding, or absent.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
        let v = Option::<serde_json::Value>::deserialize(d)?;
        match v {
            Some(v) => from_json(v).map(Some).map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

/// A [`RecordId`] as its canonical hyphenated UUIDv7 string.
pub mod record_id {
    use super::*;

    /// The canonical UUIDv7 string.
    pub fn serialize<S: Serializer>(id: &RecordId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&id.to_string())
    }

    /// The canonical UUIDv7 string.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RecordId, D::Error> {
        let text = String::deserialize(d)?;
        parse(&text).map_err(serde::de::Error::custom)
    }

    /// The id from its canonical text form: the hex digits decoded onto 16 bytes, validated by
    /// [`RecordId::from_bytes`] (version and variant), then re-checked against the exact text
    /// so only the canonical lower-case hyphenated form is accepted. The `Display` inverse,
    /// written here: this crate has no uuid dependency (the brief's list).
    pub fn parse(text: &str) -> Result<RecordId, String> {
        let bytes = text.as_bytes();
        if !uuid_shape(bytes) {
            return Err(format!(
                "`{text}` is not a record id: not canonical UUIDv7 text"
            ));
        }
        let hex = |b: u8| -> Result<u8, String> {
            match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => Err(format!("`{text}` is not a record id: not lower-case hex")),
            }
        };
        let mut raw = [0u8; 16];
        let mut di = 0usize;
        for (i, b) in bytes.iter().enumerate() {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                continue;
            }
            let nib = hex(*b)?;
            if di.is_multiple_of(2) {
                raw[di / 2] = nib << 4;
            } else {
                raw[di / 2] |= nib;
            }
            di += 1;
        }
        let rebuilt =
            RecordId::from_bytes(raw).map_err(|e| format!("`{text}` is not a record id: {e}"))?;
        if rebuilt.to_string() != text {
            return Err(format!("`{text}` is not a record id: not canonical"));
        }
        Ok(rebuilt)
    }
}

/// An optional [`RecordId`], as a string or absent.
pub mod opt_record_id {
    use super::*;

    /// The canonical UUIDv7 string, or absent.
    pub fn serialize<S: Serializer>(id: &Option<RecordId>, s: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => record_id::serialize(id, s),
            None => s.serialize_none(),
        }
    }

    /// The canonical UUIDv7 string, or absent.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RecordId>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|t| record_id::parse(&t).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// An instant: RFC 3339 in UTC.
pub mod rfc3339 {
    use super::*;

    /// RFC 3339 in UTC.
    pub fn serialize<S: Serializer>(t: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&t.format(&Rfc3339).map_err(serde::ser::Error::custom)?)
    }

    /// RFC 3339 in UTC.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
        let text = String::deserialize(d)?;
        let parsed = OffsetDateTime::parse(&text, &Rfc3339)
            .map_err(|_| serde::de::Error::custom(format!("not a valid UTC instant: `{text}`")))?;
        parsed
            .checked_to_offset(time::UtcOffset::UTC)
            .ok_or_else(|| serde::de::Error::custom(format!("not a valid UTC instant: `{text}`")))
    }
}

/// An optional instant: RFC 3339 in UTC or absent.
pub mod opt_rfc3339 {
    use super::*;

    /// RFC 3339 in UTC, or absent.
    pub fn serialize<S: Serializer>(t: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => rfc3339::serialize(t, s),
            None => s.serialize_none(),
        }
    }

    /// RFC 3339 in UTC, or absent.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|t| {
                let parsed = OffsetDateTime::parse(&t, &Rfc3339).map_err(|_| {
                    serde::de::Error::custom(format!("not a valid UTC instant: `{t}`"))
                })?;
                parsed
                    .checked_to_offset(time::UtcOffset::UTC)
                    .ok_or_else(|| {
                        serde::de::Error::custom(format!("not a valid UTC instant: `{t}`"))
                    })
            })
            .transpose()
    }
}

/// A storage [`Name`] as its string.
pub mod name {
    use super::*;

    /// The name as text.
    pub fn serialize<S: Serializer>(n: &Name, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(n.as_str())
    }

    /// The name from its text.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Name, D::Error> {
        let text = String::deserialize(d)?;
        Name::new(&text).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

/// The storage document version counter, as its number.
pub mod doc_version {
    use super::*;
    use spot_app_api::prelude::Version;

    /// The version as its number.
    pub fn serialize<S: Serializer>(v: &Version, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(v.0)
    }

    /// The version from its number.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Version, D::Error> {
        Ok(Version(u64::deserialize(d)?))
    }
}

/// The instant as its RFC 3339 text (for parameters and logs).
#[must_use]
pub fn rfc3339_text(t: &OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

/// The optional instant as JSON (a string or null), for summary parameters.
#[must_use]
pub fn opt_rfc3339_json(t: &Option<OffsetDateTime>) -> serde_json::Value {
    match t {
        Some(t) => serde_json::json!(rfc3339_text(t)),
        None => serde_json::Value::Null,
    }
}

/// Value-shape helpers for tests and for callers rebuilding a history: the same encoding,
/// called directly. `value_from_json` is the read side of [`value_to_json`].
pub mod helpers {
    use super::*;

    /// The value, as its wire JSON.
    #[must_use]
    pub fn value_to_json(v: &Value) -> serde_json::Value {
        super::value_to_json(v)
    }

    /// The value back from its wire JSON.
    pub fn value_from_json(json: &str) -> Result<Value, String> {
        let parsed: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("not JSON: {e}"))?;
        from_json(parsed)
    }
}

/// Row and table snapshots for changes that carry a whole row: JSON, in the value encoding.
pub mod row {
    use super::*;
    use spot_app_api::document::ChildRow;
    use std::collections::BTreeMap;

    /// The row, as `{"id": ..., "fields": {...}}`, fields in name order.
    #[must_use]
    pub fn to_json(row: &ChildRow) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        for (k, v) in &row.fields {
            fields.insert(k.as_str().to_owned(), crate::wire::value_to_json(v));
        }
        serde_json::json!({ "id": row.id.to_string(), "fields": fields })
    }

    /// The row from that shape; fails when the stored form is malformed. Used by the tests'
    /// round trip and by callers rebuilding a history from stored JSON.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn parse(v: serde_json::Value) -> Result<ChildRow, String> {
        let id_text = v
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or("a stored row snapshot has no id")?;
        let id = record_id::parse(id_text)
            .map_err(|e| format!("a stored row snapshot has a bad id: {e}"))?;
        let mut fields = BTreeMap::new();
        if let Some(map) = v.get("fields").and_then(serde_json::Value::as_object) {
            for (k, val) in map {
                let key = Name::new(k)
                    .map_err(|e| format!("a stored row snapshot has a bad field name: {e}"))?;
                fields.insert(key, from_json(val.clone())?);
            }
        }
        Ok(ChildRow { id, fields })
    }
}
