//! Canonical JSON: sorted keys, no whitespace, no floats.
//!
//! The golden state is committed bytes, so it is written and read in one canonical form. Two
//! properties make the comparison meaningful:
//!
//! * **Keys are sorted**, recursively, whatever `serde_json`'s map is backed by. [`canonical`]
//!   rebuilds every object in sorted order instead of trusting the default map, so a future
//!   feature flag cannot silently change the bytes.
//! * **No floating point.** ADR-012 forbids it for money, quantities and time, and a float in a
//!   document image would encode differently on another platform. A number that is not an
//!   integer (or an integer too large for `i64`/`u64`) is refused, so amounts stay the decimal
//!   **strings** the images hold and are compared exactly.

use serde_json::{Map, Value};

use crate::error::CompatError;

/// The canonical text of `value`: objects rebuilt with sorted keys, compact, no floats.
///
/// # Errors
/// [`CompatError`] when `value` holds a number that is not an integer, because a float may not
/// appear in a document image.
pub fn canonical(value: &Value) -> Result<String, CompatError> {
    serde_json::to_string(&sorted(value)?)
        .map_err(|err| CompatError::new(format!("the value cannot be encoded: {err}")))
}

/// `value` with every object's keys in ascending order, refusing a float anywhere.
///
/// # Errors
/// [`CompatError`] for a float.
pub fn sorted(value: &Value) -> Result<Value, CompatError> {
    match value {
        Value::Object(fields) => {
            let mut pairs: Vec<(&String, &Value)> = fields.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (key, field) in pairs {
                out.insert(key.clone(), sorted(field)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .map(sorted)
            .collect::<Result<Vec<Value>, CompatError>>()
            .map(Value::Array),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                Ok(Value::Number(number.clone()))
            } else {
                Err(CompatError::new(
                    "a document holds a floating-point number, which ADR-012 forbids",
                ))
            }
        }
        other => Ok(other.clone()),
    }
}

/// The JSON object `value` holds.
///
/// # Errors
/// [`CompatError`] naming `what` when it is not an object.
pub fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, CompatError> {
    value
        .as_object()
        .ok_or_else(|| CompatError::new(format!("{what} is not a JSON object")))
}

/// The unsigned integer `value` holds.
///
/// # Errors
/// [`CompatError`] naming `what` when it is not a `u64`.
pub fn unsigned(value: &Value, what: &str) -> Result<u64, CompatError> {
    value
        .as_u64()
        .ok_or_else(|| CompatError::new(format!("{what} is not an unsigned integer")))
}

/// The string `value` holds.
///
/// # Errors
/// [`CompatError`] naming `what` when it is not a string.
pub fn text<'a>(value: &'a Value, what: &str) -> Result<&'a str, CompatError> {
    value
        .as_str()
        .ok_or_else(|| CompatError::new(format!("{what} is not a string")))
}

/// The lower-case hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// The bytes `text` spells in lower-case hex, when it is exactly 64 hex digits.
#[must_use]
pub fn unhex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0_u8; 32];
    for (index, pair) in text.as_bytes().chunks(2).enumerate() {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        out[index] = u8::try_from(high * 16 + low).ok()?;
    }
    Some(out)
}
