//! Canonical JSON: the one serialisation the model digest is computed over.
//!
//! Object keys are sorted, arrays keep their order, there is no whitespace, and every number
//! is an integer (ADR-12: no floats for money, quantities or time). Two models with the same
//! facts produce the same bytes and therefore the same digest, whichever order their maps were
//! built in.

use serde::Serialize;
use serde_json::Value;

use crate::DeploymentError;

/// Serialises `value` canonically.
///
/// # Errors
///
/// [`DeploymentError::Serialise`] when the value cannot be represented as JSON.
pub(crate) fn canonical_json<T: Serialize>(value: &T) -> Result<String, DeploymentError> {
    let value = serde_json::to_value(value).map_err(|error| DeploymentError::Serialise {
        reason: error.to_string(),
    })?;
    let mut out = String::with_capacity(512);
    write_value(&value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut String) -> Result<(), DeploymentError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => out.push_str(&quote(text)?),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&quote(key)?);
                out.push(':');
                match map.get(key.as_str()) {
                    Some(item) => write_value(item, out)?,
                    // `keys` came from this map, so the lookup cannot miss; writing `null`
                    // keeps the writer total rather than panicking.
                    None => out.push_str("null"),
                }
            }
            out.push('}');
        }
    }
    Ok(())
}

fn quote(text: &str) -> Result<String, DeploymentError> {
    serde_json::to_string(text).map_err(|error| DeploymentError::Serialise {
        reason: error.to_string(),
    })
}
