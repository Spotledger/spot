//! The hand-rolled argument-schema check.
//!
//! A schema is a JSON object with at most these keywords; every one is implemented here and
//! nowhere else, so the call-time check ([`validate_arguments`]) and the authoring-time check
//! ([`check_schema`]) read the keywords through the same helpers and cannot drift:
//!
//! - `type`: one of `"object"`, `"array"`, `"string"`, `"integer"`, `"number"`, `"boolean"`,
//!   `"null"`. Nothing else has a meaning, so an unknown name is refused and the schema author
//!   fixes it.
//! - `required`: the property names that must be present (only meaningful on objects).
//! - `properties`: property name to subschema; a present property is checked against it.
//! - `additionalProperties`: `false` refuses any argument property not named in `properties`;
//!   a subschema validates every unlisted property; `true` or absent keeps every unlisted
//!   property (the old behaviour). Anything else is a schema error.
//! - `items`: the subschema every array element is checked against.
//! - `enum`: a non-empty list of `null`, boolean, string or integer values; the argument must
//!   equal one of them (JSON equality).
//! - `minLength` / `maxLength`: bounds on a string, counted in Unicode scalar values.
//! - `minimum` / `maximum`: inclusive integer bounds on a number. A bound must be an integer
//!   (a fractional bound is a schema error), and a fractional argument under a bounded schema
//!   is refused: numbers compare as integers, no float arithmetic is introduced.
//! - `minItems` / `maxItems`: bounds on an array's length.
//! - `uniqueItems`: a boolean; `true` refuses an array in which an item equals an earlier one
//!   (JSON equality: object key order is irrelevant, numbers compare as the parsed numbers),
//!   reporting the dotted path of the later item with the message `duplicate item`. The check
//!   is quadratic, so it is bounded: an array longer than [`MAX_UNIQUE_ITEMS`] under
//!   `uniqueItems: true` is refused outright. `false` or absent changes nothing.
//!
//! Any other keyword in a schema node is not understood nor looked at: arguments are never
//! rejected for keywords the check does not implement (`format`, `description`, `default`,
//! `pattern`, `oneOf`, `anyOf`, `exclusiveMinimum`, `multipleOf`, ...), and unknown keywords pass through to a later, fuller validator. One
//! [`SchemaError`] per call: the first problem found, with the dotted path inside the
//! arguments. An error never carries an argument value: only schema facts, a bound, and (for
//! an unexpected property) the property's name. No schema crate, no floats.

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

/// The type names `type` may carry.
const KINDS: [&str; 7] = [
    "object", "array", "string", "integer", "number", "boolean", "null",
];

/// The longest property name an "unexpected property" error repeats (characters); a longer
/// one is cut, so a hostile caller cannot make an error large.
const MAX_ECHOED_NAME: usize = 64;

/// The longest array a `uniqueItems: true` schema will compare pairwise; a longer array is
/// refused with a schema-level message instead of costing a quadratic walk.
pub const MAX_UNIQUE_ITEMS: usize = 1024;

type Obj = serde_json::Map<String, Json>;

/// Whether `args` matches `schema`.
///
/// # Errors
///
/// The first mismatch, as a [`SchemaError`] with the dotted path inside the arguments; also a
/// malformed keyword the walk reaches (use [`check_schema`] to find those up front).
pub fn validate_arguments(schema: &Json, args: &Json) -> Result<(), SchemaError> {
    check(schema, args, String::new())
}

/// Whether `schema` itself is well formed: an object (at every node) whose keywords are the
/// ones this module implements, with the right shapes. It walks `properties`, `items` and an
/// `additionalProperties` subschema. Unknown keywords are not examined.
///
/// The path in the error names the argument location the faulty node describes, with `*` for
/// array items.
///
/// # Errors
///
/// The first malformed node found.
pub fn check_schema(schema: &Json) -> Result<(), SchemaError> {
    shape(schema, String::new())
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

fn fail(path: &str, message: impl Into<String>) -> SchemaError {
    SchemaError {
        path: path.to_owned(),
        message: message.into(),
    }
}

fn node<'a>(schema: &'a Json, path: &str) -> Result<&'a Obj, SchemaError> {
    schema
        .as_object()
        .ok_or_else(|| fail(path, "a schema node must be a JSON object"))
}

/// The declared `type`, validated.
fn declared_type(obj: &Obj, path: &str) -> Result<Option<&'static str>, SchemaError> {
    let Some(kind) = obj.get("type") else {
        return Ok(None);
    };
    let Some(kind) = kind.as_str() else {
        return Err(fail(path, "`type` must be a string"));
    };
    match KINDS.iter().copied().find(|known| *known == kind) {
        Some(known) => Ok(Some(known)),
        None => Err(fail(path, format!("unknown schema type {kind:?}"))),
    }
}

/// The `required` names, validated.
fn required_names<'a>(obj: &'a Obj, path: &str) -> Result<Vec<&'a str>, SchemaError> {
    let Some(required) = obj.get("required") else {
        return Ok(Vec::new());
    };
    let Some(required) = required.as_array() else {
        return Err(fail(path, "`required` must be an array of property names"));
    };
    required
        .iter()
        .map(|name| {
            name.as_str()
                .ok_or_else(|| fail(path, "`required` entries must be strings"))
        })
        .collect()
}

/// The `properties` map, validated to be an object.
fn property_map<'a>(obj: &'a Obj, path: &str) -> Result<Option<&'a Obj>, SchemaError> {
    match obj.get("properties") {
        None => Ok(None),
        Some(properties) => properties
            .as_object()
            .map(Some)
            .ok_or_else(|| fail(path, "`properties` must be a JSON object")),
    }
}

/// What `additionalProperties` says.
enum Additional<'a> {
    /// Absent or `true`: unlisted properties are kept.
    Open,
    /// `false`: unlisted properties are refused.
    Closed,
    /// A subschema every unlisted property must satisfy.
    Schema(&'a Json),
}

fn additional<'a>(obj: &'a Obj, path: &str) -> Result<Additional<'a>, SchemaError> {
    match obj.get("additionalProperties") {
        None | Some(Json::Bool(true)) => Ok(Additional::Open),
        Some(Json::Bool(false)) => Ok(Additional::Closed),
        Some(sub @ Json::Object(_)) => Ok(Additional::Schema(sub)),
        Some(_) => Err(fail(
            path,
            "`additionalProperties` must be a boolean or a schema",
        )),
    }
}

/// The `enum` members, validated: a non-empty list of null, boolean, string or integer values.
fn enum_values<'a>(obj: &'a Obj, path: &str) -> Result<Option<&'a [Json]>, SchemaError> {
    let Some(values) = obj.get("enum") else {
        return Ok(None);
    };
    let Some(values) = values.as_array().filter(|values| !values.is_empty()) else {
        return Err(fail(path, "`enum` must be a non-empty array"));
    };
    let scalar = |value: &Json| match value {
        Json::Null | Json::Bool(_) | Json::String(_) => true,
        Json::Number(number) => number.is_i64() || number.is_u64(),
        Json::Array(_) | Json::Object(_) => false,
    };
    if !values.iter().all(scalar) {
        return Err(fail(
            path,
            "`enum` members must be null, boolean, string or integer values",
        ));
    }
    Ok(Some(values.as_slice()))
}

/// Whether `uniqueItems` is on, validated: absent or `false` is off, `true` is on, anything
/// else is a schema error.
fn unique_items(obj: &Obj, path: &str) -> Result<bool, SchemaError> {
    match obj.get("uniqueItems") {
        None => Ok(false),
        Some(Json::Bool(on)) => Ok(*on),
        Some(_) => Err(fail(path, "`uniqueItems` must be a boolean")),
    }
}

/// A non-negative count bound (`minLength` and its siblings), validated.
fn count_bound(obj: &Obj, key: &str, path: &str) -> Result<Option<u64>, SchemaError> {
    match obj.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| fail(path, format!("`{key}` must be a non-negative integer"))),
    }
}

/// An integer value bound (`minimum` / `maximum`), validated; a fractional bound is refused.
fn value_bound(obj: &Obj, key: &str, path: &str) -> Result<Option<i128>, SchemaError> {
    match obj.get(key) {
        None => Ok(None),
        Some(value) => match (value.as_i64(), value.as_u64()) {
            (Some(n), _) => Ok(Some(i128::from(n))),
            (None, Some(n)) => Ok(Some(i128::from(n))),
            (None, None) => Err(fail(path, format!("`{key}` must be an integer"))),
        },
    }
}

/// The paired bounds of one family, validated: each well formed and `min <= max`.
fn bounds(
    obj: &Obj,
    path: &str,
    min_key: &str,
    max_key: &str,
) -> Result<(Option<u64>, Option<u64>), SchemaError> {
    let min = count_bound(obj, min_key, path)?;
    let max = count_bound(obj, max_key, path)?;
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        return Err(fail(path, format!("`{min_key}` exceeds `{max_key}`")));
    }
    Ok((min, max))
}

fn value_bounds(obj: &Obj, path: &str) -> Result<(Option<i128>, Option<i128>), SchemaError> {
    let min = value_bound(obj, "minimum", path)?;
    let max = value_bound(obj, "maximum", path)?;
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        return Err(fail(path, "`minimum` exceeds `maximum`"));
    }
    Ok((min, max))
}

/// Every keyword of one node read and shaped, then the children.
fn shape(schema: &Json, path: String) -> Result<(), SchemaError> {
    let obj = node(schema, &path)?;
    declared_type(obj, &path)?;
    required_names(obj, &path)?;
    enum_values(obj, &path)?;
    bounds(obj, &path, "minLength", "maxLength")?;
    bounds(obj, &path, "minItems", "maxItems")?;
    value_bounds(obj, &path)?;
    unique_items(obj, &path)?;
    if let Some(properties) = property_map(obj, &path)? {
        for (name, sub) in properties {
            shape(sub, join(&path, name))?;
        }
    }
    if let Additional::Schema(sub) = additional(obj, &path)? {
        shape(sub, join(&path, "*"))?;
    }
    if let Some(items) = obj.get("items") {
        shape(items, join(&path, "*"))?;
    }
    Ok(())
}

fn check(schema: &Json, args: &Json, path: String) -> Result<(), SchemaError> {
    let obj = node(schema, &path)?;
    if let Some(kind) = declared_type(obj, &path)? {
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
            _ => false,
        };
        if !matches {
            return Err(fail(&path, format!("expected {kind}")));
        }
    }
    let map = args.as_object();
    for name in required_names(obj, &path)? {
        if !map.is_some_and(|m| m.contains_key(name)) {
            return Err(fail(&path, format!("missing required field {name:?}")));
        }
    }
    if let Some(members) = enum_values(obj, &path)?
        && !members.contains(args)
    {
        return Err(fail(&path, "not one of the allowed values"));
    }
    check_length(obj, args, &path)?;
    check_range(obj, args, &path)?;
    check_count(obj, args, &path)?;
    check_unique(obj, args, &path)?;
    let properties = property_map(obj, &path)?;
    if let Some(properties) = properties {
        for (name, sub) in properties {
            if let Some(value) = map.and_then(|m| m.get(name)) {
                let child = join(&path, name);
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
    if let Some(map) = map {
        let listed = |name: &str| properties.is_some_and(|p| p.contains_key(name));
        match additional(obj, &path)? {
            Additional::Open => {}
            Additional::Closed => {
                if let Some(name) = map.keys().find(|name| !listed(name)) {
                    return Err(fail(&join(&path, &echoed(name)), "unexpected property"));
                }
            }
            Additional::Schema(sub) => {
                for (name, value) in map {
                    if !listed(name) {
                        check(sub, value, join(&path, &echoed(name)))?;
                    }
                }
            }
        }
    }
    if let Some(items) = obj.get("items")
        && let Some(values) = args.as_array()
    {
        for (idx, value) in values.iter().enumerate() {
            check(items, value, format!("{path}.{idx}"))?;
        }
    }
    Ok(())
}

/// A property name as it may appear in an error: cut at [`MAX_ECHOED_NAME`] characters.
fn echoed(name: &str) -> String {
    if name.chars().count() <= MAX_ECHOED_NAME {
        name.to_owned()
    } else {
        let cut: String = name.chars().take(MAX_ECHOED_NAME).collect();
        format!("{cut}...")
    }
}

fn check_length(obj: &Obj, args: &Json, path: &str) -> Result<(), SchemaError> {
    let (min, max) = bounds(obj, path, "minLength", "maxLength")?;
    let Some(text) = args.as_str() else {
        return Ok(());
    };
    let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    if let Some(min) = min
        && length < min
    {
        return Err(fail(path, format!("shorter than {min} characters")));
    }
    if let Some(max) = max
        && length > max
    {
        return Err(fail(path, format!("longer than {max} characters")));
    }
    Ok(())
}

fn check_range(obj: &Obj, args: &Json, path: &str) -> Result<(), SchemaError> {
    let (min, max) = value_bounds(obj, path)?;
    if (min.is_none() && max.is_none()) || !args.is_number() {
        return Ok(());
    }
    let value = match (args.as_i64(), args.as_u64()) {
        (Some(n), _) => i128::from(n),
        (None, Some(n)) => i128::from(n),
        (None, None) => return Err(fail(path, "expected an integer")),
    };
    if let Some(min) = min
        && value < min
    {
        return Err(fail(path, format!("below the minimum of {min}")));
    }
    if let Some(max) = max
        && value > max
    {
        return Err(fail(path, format!("above the maximum of {max}")));
    }
    Ok(())
}

fn check_count(obj: &Obj, args: &Json, path: &str) -> Result<(), SchemaError> {
    let (min, max) = bounds(obj, path, "minItems", "maxItems")?;
    let Some(values) = args.as_array() else {
        return Ok(());
    };
    let count = u64::try_from(values.len()).unwrap_or(u64::MAX);
    if let Some(min) = min
        && count < min
    {
        return Err(fail(path, format!("fewer than {min} items")));
    }
    if let Some(max) = max
        && count > max
    {
        return Err(fail(path, format!("more than {max} items")));
    }
    Ok(())
}

fn check_unique(obj: &Obj, args: &Json, path: &str) -> Result<(), SchemaError> {
    let on = unique_items(obj, path)?;
    let Some(values) = args.as_array() else {
        return Ok(());
    };
    if !on {
        return Ok(());
    }
    if values.len() > MAX_UNIQUE_ITEMS {
        return Err(fail(
            path,
            format!("more than {MAX_UNIQUE_ITEMS} items cannot be checked for uniqueness"),
        ));
    }
    for (idx, value) in values.iter().enumerate() {
        if values[..idx].contains(value) {
            return Err(fail(&format!("{path}.{idx}"), "duplicate item"));
        }
    }
    Ok(())
}
