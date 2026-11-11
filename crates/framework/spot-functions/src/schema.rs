//! The hand-rolled argument-schema check: `type`, `required` and `properties` only.
//!
//! A schema is a JSON object with at most these keywords:
//!
//! - `type`: one of `"object"`, `"array"`, `"string"`, `"integer"`, `"number"`, `"boolean"`,
//!   `"null"`. Nothing else has a meaning, so an unknown name is refused and the schema author
//!   fixes it.
//! - `required`: the property names that must be present (only meaningful on objects).
//! - `properties`: property name to subschema; a present property is checked against it.
//!
//! Anything else in a schema node is not understood nor looked at: arguments are never
//! rejected for keywords the check does not implement, and unknown keywords pass through to a
//! later, fuller validator. One [`SchemaError`] per call: the first problem found, with the
//! dotted path inside the arguments. No schema crate, no floats.

use std::fmt;

use serde_json::Value as Json;

/// Why arguments failed their schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaError {
    /// The dotted path inside the arguments, empty at the top level; array indexes join as
    /// `.0`, so `items.2.qty` for the third item's `qty`.
    pub path: String,
    /// What is wrong, without any argument content.
    pub message: String,
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for SchemaError {}

/// Whether `args` matches `schema` (`type`/`required`/`properties` only).
pub fn validate_arguments(schema: &Json, args: &Json) -> Result<(), SchemaError> {
    check(schema, args, String::new())
}

fn check(schema: &Json, args: &Json, path: String) -> Result<(), SchemaError> {
    let Some(obj) = schema.as_object() else {
        return Err(SchemaError {
            path,
            message: "a schema node must be a JSON object".to_owned(),
        });
    };
    if let Some(kind) = obj.get("type") {
        let Some(kind) = kind.as_str() else {
            return Err(SchemaError {
                path,
                message: "`type` must be a string".to_owned(),
            });
        };
        let matches = match kind {
            "object" => args.is_object(),
            "array" => args.is_array(),
            "string" => args.is_string(),
            "integer" => args.is_i64() || args.is_u64(),
            "number" => {
                args.is_i64() || args.is_u64() || args.as_f64().is_some_and(|f| f.is_finite())
            }
            "boolean" => args.is_boolean(),
            "null" => args.is_null(),
            other => {
                return Err(SchemaError {
                    path,
                    message: format!("unknown schema type {other:?}"),
                });
            }
        };
        if !matches {
            return Err(SchemaError {
                path,
                message: format!("expected {kind}"),
            });
        }
    }
    if let Some(required) = obj.get("required") {
        let Some(required) = required.as_array() else {
            return Err(SchemaError {
                path,
                message: "`required` must be an array of property names".to_owned(),
            });
        };
        let map = args.as_object();
        for name in required {
            let Some(name) = name.as_str() else {
                return Err(SchemaError {
                    path: path.clone(),
                    message: "`required` entries must be strings".to_owned(),
                });
            };
            if !map.is_some_and(|m| m.contains_key(name)) {
                return Err(SchemaError {
                    path: path.clone(),
                    message: format!("missing required field {name:?}"),
                });
            }
        }
    }
    if let Some(properties) = obj.get("properties") {
        let Some(properties) = properties.as_object() else {
            return Err(SchemaError {
                path,
                message: "`properties` must be a JSON object".to_owned(),
            });
        };
        let map = args.as_object();
        for (name, sub) in properties {
            if let Some(value) = map.and_then(|m| m.get(name)) {
                let child = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}.{name}")
                };
                if args.is_array() {
                    // An array schema's properties are positional; unreachable in practice for
                    // object properties, but kept so arrays never loop.
                    check(sub, value, format!("{child}.0"))?;
                } else {
                    check(sub, value, child)?;
                }
            }
        }
    }
    if let Some(items) = obj.get("items").filter(|_| args.is_array()) {
        let Some(values) = args.as_array() else {
            return Ok(());
        };
        for (idx, value) in values.iter().enumerate() {
            check(items, value, format!("{path}.{idx}"))?;
        }
    }
    Ok(())
}
