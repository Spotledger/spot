//! The small type DSL a module declares its method arguments and results with.
//!
//! A method declares its arguments as an object mapping a name to a type tag:
//!
//! ```text
//! args: { amount: "decimal", note: "text?" }
//! ```
//!
//! and its result as a single tag (`"text"`, `"decimal[]"`) or an object of named tags. The crate
//! turns a declaration into the **same JSON Schema** the native `#[whitelist]` macro generates
//! (WP-DOC-11.T1), so the API layer serves one shape whatever a controller's source is, and
//! validates the arguments **before** a handler runs. A decimal crosses the bridge as its exact
//! string and is never a JavaScript number (ADR-12).

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json, json};
use spot_app_api::error::FieldPath;
use spot_app_api::{AppError, AppResult, ErrorClass};

use crate::problem::Problem;

/// The code the native API uses when arguments do not fit a method's declaration. A TypeScript
/// method reports the same code a native one does.
pub const BAD_ARGUMENTS: &str = "whitelist.bad_arguments";

/// A tag's base type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldType {
    /// A string.
    Text,
    /// An integer.
    Int,
    /// An exact decimal, carried as its string (ADR-12).
    Decimal,
    /// `true` or `false`.
    Bool,
    /// A calendar date (`YYYY-MM-DD`).
    Date,
    /// An instant (RFC 3339).
    DateTime,
    /// A time of day (`HH:MM:SS`).
    Time,
    /// A record id (UUIDv7).
    Id,
    /// Arbitrary JSON.
    Json,
    /// Anything, including `null`.
    Any,
    /// Exactly `null` (the unit result).
    Null,
}

/// One parsed type tag: a base type, whether it may be `null`, and whether it is an array of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeTag {
    /// The base type.
    pub ty: FieldType,
    /// Whether the tag carried a trailing `?`.
    pub optional: bool,
    /// Whether the tag carried a trailing `[]`.
    pub array: bool,
}

/// Parses a type tag such as `"decimal"`, `"text?"` or `"decimal[]"`.
///
/// # Errors
///
/// A sentence naming what is wrong with the tag.
pub fn parse_tag(text: &str) -> Result<TypeTag, String> {
    let mut rest = text.trim();
    let mut optional = false;
    let mut array = false;
    if let Some(stripped) = rest.strip_suffix('?') {
        optional = true;
        rest = stripped.trim_end();
    }
    if let Some(stripped) = rest.strip_suffix("[]") {
        array = true;
        rest = stripped.trim_end();
    }
    let ty = match rest {
        "text" | "string" => FieldType::Text,
        "int" | "integer" => FieldType::Int,
        "decimal" | "money" | "number" => FieldType::Decimal,
        "bool" | "boolean" => FieldType::Bool,
        "date" => FieldType::Date,
        "datetime" | "date-time" => FieldType::DateTime,
        "time" => FieldType::Time,
        "id" | "link" | "record-id" => FieldType::Id,
        "json" | "value" => FieldType::Json,
        "any" => FieldType::Any,
        "null" | "unit" => FieldType::Null,
        other => {
            return Err(format!(
                "`{other}` is not a type tag; use text, int, decimal, bool, date, datetime, \
                 time, id, json, any or null, with an optional `?` and `[]`"
            ));
        }
    };
    Ok(TypeTag {
        ty,
        optional,
        array,
    })
}

/// The JSON Schema of one tag, the same shape the native macro emits.
pub fn tag_schema(tag: TypeTag) -> Json {
    let inner = base_schema(tag.ty);
    let inner = if tag.optional {
        json!({ "anyOf": [inner, { "type": "null" }] })
    } else {
        inner
    };
    if tag.array {
        json!({ "type": "array", "items": inner })
    } else {
        inner
    }
}

fn base_schema(ty: FieldType) -> Json {
    match ty {
        FieldType::Text => json!({ "type": "string" }),
        FieldType::Int => json!({ "type": "integer" }),
        FieldType::Decimal => json!({ "type": "string", "format": "decimal" }),
        FieldType::Bool => json!({ "type": "boolean" }),
        FieldType::Date => json!({ "type": "string", "format": "date" }),
        FieldType::DateTime => json!({ "type": "string", "format": "date-time" }),
        FieldType::Time => json!({ "type": "string", "format": "time" }),
        FieldType::Id => json!({ "type": "string", "format": "record-id" }),
        FieldType::Json | FieldType::Any => json!({}),
        FieldType::Null => json!({ "type": "null" }),
    }
}

/// A method's argument declaration: named tags, which are required, and whether unknown names are
/// allowed (`additionalProperties`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgsSchema {
    /// The declared arguments, in name order.
    pub properties: BTreeMap<String, TypeTag>,
    /// The arguments that must be present (every non-optional one, in name order).
    pub required: Vec<String>,
    /// Whether a name outside [`ArgsSchema::properties`] is allowed.
    pub open: bool,
}

impl ArgsSchema {
    /// The empty declaration (a method that takes no arguments).
    pub fn empty() -> ArgsSchema {
        ArgsSchema {
            properties: BTreeMap::new(),
            required: Vec::new(),
            open: false,
        }
    }

    /// The JSON Schema of the declaration.
    pub fn schema(&self) -> Json {
        let mut properties = Map::new();
        for (name, tag) in &self.properties {
            properties.insert(name.clone(), tag_schema(*tag));
        }
        json!({
            "type": "object",
            "properties": Json::Object(properties),
            "required": self.required,
            "additionalProperties": self.open,
        })
    }

    /// Reads a declaration from the JSON a module's `contract()` returned. `$open` (a boolean)
    /// allows unknown names; any other name is an argument.
    ///
    /// # Errors
    ///
    /// Every problem at once, each at `steps.<...>.args.<name>`.
    pub fn from_json(value: &Json, path: &str) -> Result<ArgsSchema, Vec<Problem>> {
        let mut problems = Vec::new();
        let mut schema = ArgsSchema::empty();
        let Some(object) = value.as_object() else {
            problems.push(Problem::new(
                crate::codes::INVALID_MODULE,
                path,
                "`args` must be an object mapping an argument name to a type tag",
            ));
            return Err(problems);
        };
        for (name, tag_value) in object {
            if name == "$open" {
                match tag_value.as_bool() {
                    Some(open) => schema.open = open,
                    None => problems.push(Problem::new(
                        crate::codes::INVALID_MODULE,
                        &format!("{path}.$open"),
                        "`$open` must be true or false",
                    )),
                }
                continue;
            }
            if spot_storage_contract::Name::new(name).is_err() {
                problems.push(Problem::new(
                    crate::codes::INVALID_MODULE,
                    &format!("{path}.{name}"),
                    format!("`{name}` is not a storage-style argument name"),
                ));
                continue;
            }
            let Some(text) = tag_value.as_str() else {
                problems.push(Problem::new(
                    crate::codes::INVALID_MODULE,
                    &format!("{path}.{name}"),
                    "a type tag is a string",
                ));
                continue;
            };
            match parse_tag(text) {
                Ok(tag) => {
                    if !tag.optional {
                        schema.required.push(name.clone());
                    }
                    schema.properties.insert(name.clone(), tag);
                }
                Err(why) => problems.push(Problem::new(
                    crate::codes::INVALID_MODULE,
                    &format!("{path}.{name}"),
                    why,
                )),
            }
        }
        schema.required.sort();
        if problems.is_empty() {
            Ok(schema)
        } else {
            Err(problems)
        }
    }
}

/// The JSON Schema of a declared result: a single tag (`"decimal[]"`), an object of named tags, or
/// `{}` (anything) when the declaration is missing.
///
/// # Errors
///
/// Every problem at once, each at `<path>` or `<path>.<name>`.
pub fn result_schema_from_json(value: Option<&Json>, path: &str) -> Result<Json, Vec<Problem>> {
    let Some(value) = value else {
        return Ok(json!({}));
    };
    if value.is_null() {
        return Ok(json!({}));
    }
    if let Some(text) = value.as_str() {
        return parse_tag(text)
            .map(tag_schema)
            .map_err(|why| vec![Problem::new(crate::codes::INVALID_MODULE, path, why)]);
    }
    let Some(object) = value.as_object() else {
        return Err(vec![Problem::new(
            crate::codes::INVALID_MODULE,
            path,
            "`result` must be a type tag string or an object of named tags",
        )]);
    };
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut problems = Vec::new();
    for (name, tag_value) in object {
        if name == "$open" {
            continue;
        }
        let Some(text) = tag_value.as_str() else {
            problems.push(Problem::new(
                crate::codes::INVALID_MODULE,
                &format!("{path}.{name}"),
                "a type tag is a string",
            ));
            continue;
        };
        match parse_tag(text) {
            Ok(tag) => {
                if !tag.optional {
                    required.push(name.clone());
                }
                properties.insert(name.clone(), tag_schema(tag));
            }
            Err(why) => problems.push(Problem::new(
                crate::codes::INVALID_MODULE,
                &format!("{path}.{name}"),
                why,
            )),
        }
    }
    required.sort();
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(json!({
        "type": "object",
        "properties": Json::Object(properties),
        "required": required,
        "additionalProperties": false,
    }))
}

/// Checks `args` against a method's declaration, before its handler runs.
///
/// # Errors
///
/// `whitelist.bad_arguments`, as a field error naming the first bad argument (the same error and
/// code a native `#[whitelist]` method raises).
pub fn validate_args(schema: &ArgsSchema, args: &Json) -> AppResult<()> {
    let empty = Map::new();
    let given = match args {
        Json::Null => &empty,
        Json::Object(object) => object,
        _ => {
            return Err(bad(
                None,
                "the arguments must be an object or null".to_owned(),
            ));
        }
    };
    if !schema.open {
        for key in given.keys() {
            if !schema.properties.contains_key(key) {
                return Err(bad(Some(key), format!("unknown argument `{key}`")));
            }
        }
    }
    for name in &schema.required {
        if !given.contains_key(name) {
            return Err(bad(Some(name), format!("missing argument `{name}`")));
        }
    }
    for (name, value) in given {
        if let Some(tag) = schema.properties.get(name)
            && let Some(why) = tag_violation(*tag, value)
        {
            return Err(bad(Some(name), why));
        }
    }
    Ok(())
}

/// Why `value` does not have `tag`, or `None` when it does.
pub fn tag_violation(tag: TypeTag, value: &Json) -> Option<String> {
    if tag.array {
        let items = value.as_array()?;
        return items.iter().find_map(|item| {
            let element = TypeTag {
                array: false,
                ..tag
            };
            tag_violation(element, item)
        });
    }
    if value.is_null() {
        return if tag.optional || matches!(tag.ty, FieldType::Any | FieldType::Null) {
            None
        } else {
            Some("a value is required".to_owned())
        };
    }
    let ok = match tag.ty {
        FieldType::Any | FieldType::Json => true,
        FieldType::Text => value.is_string(),
        FieldType::Int => value.is_i64() || value.is_u64(),
        FieldType::Bool => value.is_boolean(),
        FieldType::Decimal => value
            .as_str()
            .is_some_and(|text| spot_money::rounding::parse_decimal(text).is_ok()),
        FieldType::Date | FieldType::Time | FieldType::DateTime => value.is_string(),
        FieldType::Id => value
            .as_str()
            .is_some_and(|text| crate::bridge::parse_record_id(text).is_some()),
        FieldType::Null => false,
    };
    if ok {
        None
    } else {
        Some(format!("expected {}", describe(tag.ty)))
    }
}

fn describe(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Text => "a string",
        FieldType::Int => "an integer",
        FieldType::Decimal => "a decimal string",
        FieldType::Bool => "true or false",
        FieldType::Date => "a date, `YYYY-MM-DD`",
        FieldType::DateTime => "an RFC 3339 timestamp",
        FieldType::Time => "a time, `HH:MM:SS`",
        FieldType::Id => "a record id",
        FieldType::Json => "JSON",
        FieldType::Any => "anything",
        FieldType::Null => "null",
    }
}

/// Checks a handler's result against the declared result schema.
///
/// # Errors
///
/// A sentence naming the first value that does not fit.
pub fn validate_result(schema: &Json, value: &Json) -> Result<(), String> {
    if schema.as_object().is_some_and(Map::is_empty) {
        return Ok(());
    }
    if let Some(any_of) = schema.get("anyOf").and_then(Json::as_array) {
        return if any_of
            .iter()
            .any(|option| validate_result(option, value).is_ok())
        {
            Ok(())
        } else {
            Err("the value does not fit any allowed shape".to_owned())
        };
    }
    match schema.get("type").and_then(Json::as_str) {
        None => Ok(()),
        Some("null") => {
            if value.is_null() {
                Ok(())
            } else {
                Err("expected null".to_owned())
            }
        }
        Some("string") => {
            if value.is_string() {
                Ok(())
            } else {
                Err("expected a string".to_owned())
            }
        }
        Some("integer") => {
            if value.is_i64() || value.is_u64() {
                Ok(())
            } else {
                Err("expected an integer".to_owned())
            }
        }
        Some("boolean") => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err("expected true or false".to_owned())
            }
        }
        Some("array") => {
            let Some(items) = value.as_array() else {
                return Err("expected an array".to_owned());
            };
            let item_schema = schema.get("items").cloned().unwrap_or_else(|| json!({}));
            for item in items {
                validate_result(&item_schema, item)?;
            }
            Ok(())
        }
        Some("object") => {
            let Some(object) = value.as_object() else {
                return Err("expected an object".to_owned());
            };
            let properties = schema.get("properties").and_then(Json::as_object);
            let required = schema.get("required").and_then(Json::as_array);
            if let Some(required) = required {
                for name in required.iter().filter_map(Json::as_str) {
                    if !object.contains_key(name) {
                        return Err(format!("the result is missing `{name}`"));
                    }
                }
            }
            if let (Some(properties), false) = (
                properties,
                schema
                    .get("additionalProperties")
                    .and_then(Json::as_bool)
                    .unwrap_or(true),
            ) {
                for key in object.keys() {
                    if !properties.contains_key(key) {
                        return Err(format!("the result has an undeclared field `{key}`"));
                    }
                }
            }
            if let Some(properties) = properties {
                for (name, subschema) in properties {
                    if let Some(field) = object.get(name) {
                        validate_result(subschema, field)
                            .map_err(|why| format!("`{name}`: {why}"))?;
                    }
                }
            }
            Ok(())
        }
        Some(other) => Err(format!("expected {other}")),
    }
}

/// The `whitelist.bad_arguments` error for a method's arguments.
fn bad(field: Option<&str>, detail: String) -> AppError {
    let message = format!("the arguments are not valid: {detail}");
    match field.and_then(FieldPath::parse) {
        Some(path) => AppError::field(path, BAD_ARGUMENTS, message),
        None => AppError::new(ErrorClass::Validation, BAD_ARGUMENTS, message),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn tags_parse_and_map_to_the_native_schema() {
        assert_eq!(
            tag_schema(parse_tag("text").unwrap()),
            json!({"type": "string"})
        );
        assert_eq!(
            tag_schema(parse_tag("decimal").unwrap()),
            json!({"type": "string", "format": "decimal"})
        );
        assert_eq!(
            tag_schema(parse_tag("text?").unwrap()),
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        );
        assert_eq!(
            tag_schema(parse_tag("int[]").unwrap()),
            json!({"type": "array", "items": {"type": "integer"}})
        );
        assert!(parse_tag("nope").is_err());
    }

    #[test]
    fn an_args_declaration_becomes_the_macro_schema() {
        let schema = ArgsSchema::from_json(
            &json!({"amount": "decimal", "note": "text?"}),
            "methods.approve.args",
        )
        .unwrap();
        assert_eq!(schema.required, vec!["amount".to_owned()]);
        assert_eq!(
            schema.schema(),
            json!({
                "type": "object",
                "properties": {
                    "amount": {"type": "string", "format": "decimal"},
                    "note": {"anyOf": [{"type": "string"}, {"type": "null"}]}
                },
                "required": ["amount"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn a_decimal_is_a_string_and_a_number_is_refused() {
        let schema = ArgsSchema::from_json(&json!({"amount": "decimal"}), "args").unwrap();
        assert!(validate_args(&schema, &json!({"amount": "12.50"})).is_ok());
        let error = validate_args(&schema, &json!({"amount": 12.5})).unwrap_err();
        assert_eq!(error.class, ErrorClass::Validation);
        assert_eq!(error.fields[0].code, BAD_ARGUMENTS);
    }

    #[test]
    fn unknown_and_missing_arguments_are_field_errors() {
        let schema = ArgsSchema::from_json(&json!({"title": "text"}), "args").unwrap();
        let unknown = validate_args(&schema, &json!({"title": "x", "extra": 1})).unwrap_err();
        assert_eq!(unknown.fields[0].code, BAD_ARGUMENTS);
        let missing = validate_args(&schema, &json!({})).unwrap_err();
        assert_eq!(missing.fields[0].code, BAD_ARGUMENTS);
    }

    #[test]
    fn a_result_is_checked_against_its_schema() {
        let schema = result_schema_from_json(Some(&json!({"ok": "bool"})), "result").unwrap();
        assert!(validate_result(&schema, &json!({"ok": true})).is_ok());
        assert!(validate_result(&schema, &json!({"ok": "yes"})).is_err());
        assert!(validate_result(&schema, &json!({})).is_err());
        assert!(validate_result(&json!({}), &json!({"anything": 1})).is_ok());
    }
}
