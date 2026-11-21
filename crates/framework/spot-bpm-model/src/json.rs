//! JSON helpers: canonical serialisation, float detection and the TOML-to-JSON
//! conversion used to preserve an unknown node's config.
//!
//! Canonical JSON is what a content hash and a run's variables are written in:
//! object keys sorted, no whitespace, integers only (ADR-12 — money,
//! quantities and time are never floats).

use serde_json::Value;

/// The path of the first float inside `value`, or `None` when there is none.
///
/// A JSON number counts as a float when it is not an integer (`1` passes,
/// `1.5` fails). Nested arrays and objects produce `items[2].amount`-style
/// paths.
#[must_use]
pub fn find_float(value: &Value, path: &str) -> Option<String> {
    match value {
        Value::Number(n) if n.as_i64().is_none() => Some(path.to_owned()),
        Value::Array(items) => items.iter().enumerate().find_map(|(i, item)| {
            let child = format!("{path}[{i}]");
            find_float(item, &child)
        }),
        Value::Object(map) => map.iter().find_map(|(k, item)| {
            let child = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            find_float(item, &child)
        }),
        _ => None,
    }
}

/// Serialises a JSON value canonically: object keys sorted, arrays in order, no
/// whitespace, numbers as serde_json wrote them (the model rejects floats).
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

/// Writes one value into `out` in canonical form.
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => {
            // serde_json's own writer for scalars cannot carry a string with a
            // newline unseen; a scalar has no float by contract, so this is the
            // shortest exact form.
            out.push_str(&serde_json::to_string(other).unwrap_or_default());
        }
    }
}

/// Writes a JSON string literal (with the standard escapes).
fn write_json_string(text: &str, out: &mut String) {
    out.push_str(&serde_json::to_string(text).unwrap_or_default());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_strips_whitespace() {
        let value: Value =
            serde_json::from_str(r#"{ "b": 2, "a": [1, 2], "c": {"z": 1} }"#).unwrap();
        assert_eq!(canonical_json(&value), r#"{"a":[1,2],"b":2,"c":{"z":1}}"#);
    }

    #[test]
    fn floats_are_found_at_a_path() {
        let value: Value =
            serde_json::from_str(r#"{ "items": [ {"qty": 1}, {"qty": 2.5} ] }"#).unwrap();
        assert_eq!(find_float(&value, ""), Some("items[1].qty".to_owned()));
    }

    #[test]
    fn integers_are_not_floats() {
        let value: Value = serde_json::from_str(r#"{ "qty": 5 }"#).unwrap();
        assert_eq!(find_float(&value, ""), None);
    }
}
