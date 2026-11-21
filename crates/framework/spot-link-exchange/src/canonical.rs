//! Canonical JSON for message payloads.
//!
//! A payload is signed as text, so both sides must agree on one spelling: object keys in byte
//! order, no whitespace, integers only. A float never crosses a link (ADR-12): money and
//! quantities travel as decimal text or minor units.

use serde_json::Value;

use crate::error::{Error, Result};

/// The largest payload, in bytes, a message may carry.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

const MAX_DEPTH: usize = 32;

/// Parses `text` and returns its canonical spelling.
///
/// # Errors
/// [`Error::InvalidPayload`] when the text is too large, is not JSON, nests deeper than 32
/// levels or holds a non-integer number.
pub fn canonicalize(text: &str) -> Result<String> {
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::InvalidPayload {
            reason: "the payload is over the size cap",
        });
    }
    let value: Value = serde_json::from_str(text).map_err(|_| Error::InvalidPayload {
        reason: "the payload is not JSON",
    })?;
    let mut out = String::with_capacity(text.len());
    write(&value, 0, &mut out)?;
    if out.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::InvalidPayload {
            reason: "the payload is over the size cap",
        });
    }
    Ok(out)
}

/// Whether `text` is already in canonical spelling.
#[must_use]
pub fn is_canonical(text: &str) -> bool {
    canonicalize(text).is_ok_and(|canonical| canonical == text)
}

fn write(value: &Value, depth: usize, out: &mut String) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::InvalidPayload {
            reason: "the payload nests too deeply",
        });
    }
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if number.is_f64() {
                return Err(Error::InvalidPayload {
                    reason: "the payload holds a non-integer number",
                });
            }
            out.push_str(&number.to_string());
        }
        Value::String(text) => push_string(text, out)?,
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write(item, depth + 1, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_string(key, out)?;
                out.push(':');
                if let Some(item) = map.get(key) {
                    write(item, depth + 1, out)?;
                }
            }
            out.push('}');
        }
    }
    Ok(())
}

fn push_string(text: &str, out: &mut String) -> Result<()> {
    let quoted = serde_json::to_string(text).map_err(|_| Error::InvalidPayload {
        reason: "the payload holds unencodable text",
    })?;
    out.push_str(&quoted);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_sorted_and_whitespace_dropped() {
        let text =
            canonicalize(r#"{ "b": 1, "a": {"d": [true, null], "c": "x"} }"#).unwrap_or_default();
        assert_eq!(text, r#"{"a":{"c":"x","d":[true,null]},"b":1}"#);
        assert!(is_canonical(&text));
    }

    #[test]
    fn a_float_is_refused() {
        assert!(canonicalize(r#"{"a":1.5}"#).is_err());
        assert!(canonicalize(r#"{"a":1e3}"#).is_err());
    }

    #[test]
    fn size_and_depth_are_capped() {
        let big = format!("\"{}\"", "x".repeat(MAX_PAYLOAD_BYTES));
        assert!(canonicalize(&big).is_err());
        let deep = format!("{}1{}", "[".repeat(40), "]".repeat(40));
        assert!(canonicalize(&deep).is_err());
    }
}
