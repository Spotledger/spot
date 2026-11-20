//! [`validate_input`]: checks a manual or API start's input against the trigger's JSON schema.
//!
//! The check understands the subset a start form needs: `type`, `required`, `properties`,
//! `additionalProperties` (a boolean), `enum`, `items`, the integer bounds `minimum` and
//! `maximum`, and the string lengths `minLength` and `maxLength` (in characters). `pattern` is
//! not supported (no regular-expression engine here), so a schema that uses it is refused. A schema keyword outside that subset
//! is reported as a problem rather than ignored, so a schema this crate cannot enforce refuses
//! the start (fail closed) instead of silently accepting any input.
//!
//! Every problem is listed, not only the first. A problem names the JSON pointer of the value and
//! the fault kind only; it never echoes the caller's value.

use serde_json::Value as Json;

/// Keywords that carry no constraint.
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "title",
    "description",
    "default",
    "examples",
];

/// Every problem `input` has against `schema`; empty when the input is valid.
#[must_use]
pub fn validate_input(schema: &Json, input: &Json) -> Vec<String> {
    let mut problems = Vec::new();
    check(schema, input, "", &mut problems);
    problems
}

/// Every reason `schema` cannot be enforced by [`validate_input`]; empty when it can.
///
/// A problem names a JSON pointer into the schema and a fault kind only, never a keyword name
/// or a value from the schema (it is a definition author's text, reported without echo).
#[must_use]
pub fn check_schema(schema: &Json) -> Vec<String> {
    let mut problems = Vec::new();
    check_schema_at(schema, "", &mut problems);
    problems
}

fn check_schema_at(schema: &Json, path: &str, problems: &mut Vec<String>) {
    let at = pointer(path);
    let rules = match schema {
        Json::Bool(_) => return,
        Json::Object(rules) => rules,
        _ => {
            problems.push(format!("{at}: the schema is not an object"));
            return;
        }
    };
    for (keyword, value) in rules {
        match keyword.as_str() {
            "type" => {
                let names: Option<Vec<&str>> = match value {
                    Json::String(name) => Some(vec![name.as_str()]),
                    Json::Array(items) => items.iter().map(Json::as_str).collect(),
                    _ => None,
                };
                let ok = names.is_some_and(|names| {
                    !names.is_empty()
                        && names.iter().all(|n| type_matches(n, &Json::Null).is_some())
                });
                if !ok {
                    problems.push(format!("{at}: the schema `type` is malformed"));
                }
            }
            "required" => {
                let ok = value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Json::is_string));
                if !ok {
                    problems.push(format!("{at}: the schema `required` is malformed"));
                }
            }
            "properties" => match value.as_object() {
                Some(fields) => {
                    for (name, sub) in fields {
                        let child =
                            format!("{path}/{}", name.replace('~', "~0").replace('/', "~1"));
                        check_schema_at(sub, &child, problems);
                    }
                }
                None => problems.push(format!("{at}: the schema `properties` is malformed")),
            },
            "additionalProperties" => {
                if !value.is_boolean() {
                    problems.push(format!(
                        "{at}: the schema `additionalProperties` must be true or false"
                    ));
                }
            }
            "enum" => {
                if !value.is_array() {
                    problems.push(format!("{at}: the schema `enum` is malformed"));
                }
            }
            "minimum" | "maximum" => {
                if value.as_i64().is_none() {
                    problems.push(format!("{at}: a schema bound must be an integer"));
                }
            }
            "minLength" | "maxLength" => {
                if value.as_u64().is_none() {
                    problems.push(format!(
                        "{at}: a schema length must be a non-negative integer"
                    ));
                }
            }
            "items" => check_schema_at(value, &format!("{path}/items"), problems),
            other if ANNOTATIONS.contains(&other) => {}
            _ => problems.push(format!("{at}: a schema keyword is not supported")),
        }
    }
}

fn pointer(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

fn type_matches(name: &str, value: &Json) -> Option<bool> {
    Some(match name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => return None,
    })
}

/// `minimum`/`maximum` on a number and `minLength`/`maxLength` on a string; a bound the schema
/// states wrongly is reported by [`check_schema`], so here it is simply not applied.
fn check_bounds(
    rules: &serde_json::Map<String, Json>,
    value: &Json,
    at: &str,
    problems: &mut Vec<String>,
) {
    if let Json::Number(number) = value {
        let below = |bound: i64| match (number.as_i64(), number.as_u64()) {
            (Some(n), _) => n < bound,
            (None, Some(_)) => false,
            (None, None) => number.as_f64().is_some_and(|f| f < bound as f64),
        };
        let above = |bound: i64| match (number.as_i64(), number.as_u64()) {
            (Some(n), _) => n > bound,
            (None, Some(_)) => true,
            (None, None) => number.as_f64().is_some_and(|f| f > bound as f64),
        };
        if let Some(bound) = rules.get("minimum").and_then(Json::as_i64)
            && below(bound)
        {
            problems.push(format!("{at}: below the minimum"));
        }
        if let Some(bound) = rules.get("maximum").and_then(Json::as_i64)
            && above(bound)
        {
            problems.push(format!("{at}: above the maximum"));
        }
    }
    if let Json::String(text) = value {
        let length = text.chars().count() as u64;
        if let Some(bound) = rules.get("minLength").and_then(Json::as_u64)
            && length < bound
        {
            problems.push(format!("{at}: too short"));
        }
        if let Some(bound) = rules.get("maxLength").and_then(Json::as_u64)
            && length > bound
        {
            problems.push(format!("{at}: too long"));
        }
    }
}

fn check(schema: &Json, value: &Json, path: &str, problems: &mut Vec<String>) {
    let at = pointer(path);
    let rules = match schema {
        Json::Bool(true) => return,
        Json::Bool(false) => {
            problems.push(format!("{at}: no value is allowed here"));
            return;
        }
        Json::Object(rules) => rules,
        _ => {
            problems.push(format!("{at}: the schema is not an object"));
            return;
        }
    };
    for keyword in rules.keys() {
        let known = matches!(
            keyword.as_str(),
            "type"
                | "required"
                | "properties"
                | "additionalProperties"
                | "enum"
                | "items"
                | "minimum"
                | "maximum"
                | "minLength"
                | "maxLength"
        ) || ANNOTATIONS.contains(&keyword.as_str());
        if !known {
            problems.push(format!("{at}: schema keyword `{keyword}` is not supported"));
        }
    }
    if let Some(declared) = rules.get("type") {
        let names: Vec<&str> = match declared {
            Json::String(name) => vec![name.as_str()],
            Json::Array(items) => items.iter().filter_map(Json::as_str).collect(),
            _ => Vec::new(),
        };
        let mut known = names.is_empty();
        let mut ok = false;
        for name in &names {
            match type_matches(name, value) {
                Some(hit) => {
                    known = true;
                    ok |= hit;
                }
                None => problems.push(format!("{at}: unknown schema type `{name}`")),
            }
        }
        if !known {
            problems.push(format!("{at}: the schema `type` is malformed"));
        } else if !ok && !names.is_empty() {
            problems.push(format!("{at}: wrong type, expected {}", names.join(" or ")));
            return;
        }
    }
    check_bounds(rules, value, at, problems);
    if let Some(Json::Array(allowed)) = rules.get("enum")
        && !allowed.contains(value)
    {
        problems.push(format!("{at}: not one of the allowed values"));
    }
    if let Json::Object(fields) = value {
        if let Some(Json::Array(required)) = rules.get("required") {
            for name in required.iter().filter_map(Json::as_str) {
                if !fields.contains_key(name) {
                    problems.push(format!("{}/{name}: required and missing", path));
                }
            }
        }
        let properties = rules.get("properties").and_then(Json::as_object);
        for (name, field) in fields {
            let child = format!("{path}/{}", name.replace('~', "~0").replace('/', "~1"));
            match properties.and_then(|known| known.get(name)) {
                Some(sub) => check(sub, field, &child, problems),
                None if rules.get("additionalProperties") == Some(&Json::Bool(false)) => {
                    problems.push(format!("{child}: not an allowed field"));
                }
                None => {}
            }
        }
    }
    if let (Json::Array(items), Some(sub)) = (value, rules.get("items")) {
        for (index, item) in items.iter().enumerate() {
            check(sub, item, &format!("{path}/{index}"), problems);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Json {
        json!({
            "type": "object",
            "required": ["customer", "qty"],
            "additionalProperties": false,
            "properties": {
                "customer": {"type": "string"},
                "qty": {"type": "integer"},
                "kind": {"enum": ["a", "b"]},
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        })
    }

    #[test]
    fn a_valid_input_has_no_problem() {
        let input = json!({"customer": "c1", "qty": 3, "kind": "a", "tags": ["x"]});
        assert!(validate_input(&schema(), &input).is_empty());
    }

    #[test]
    fn every_problem_is_listed_and_no_value_is_echoed() {
        let input = json!({"qty": "secret-text", "kind": "z", "extra": 1, "tags": [1, "x", 2]});
        let problems = validate_input(&schema(), &input);
        assert_eq!(problems.len(), 6, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("/customer: required")));
        assert!(problems.iter().any(|p| p.starts_with("/qty: wrong type")));
        assert!(problems.iter().any(|p| p.starts_with("/kind: not one of")));
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("/extra: not an allowed"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("/tags/0: wrong type"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("/tags/2: wrong type"))
        );
        assert!(problems.iter().all(|p| !p.contains("secret-text")));
    }

    #[test]
    fn a_non_object_input_is_refused_by_an_object_schema() {
        assert_eq!(validate_input(&schema(), &json!("text")).len(), 1);
        assert_eq!(validate_input(&schema(), &Json::Null).len(), 1);
    }

    #[test]
    fn check_schema_accepts_the_enforceable_subset_and_names_no_keyword() {
        assert!(check_schema(&schema()).is_empty());
        let problems = check_schema(&json!({
            "type": "object",
            "properties": {"a": {"pattern": "^secret"}, "b": {"type": "wat"}},
            "additionalProperties": "no"
        }));
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().all(|p| !p.contains("pattern")));
        assert!(problems.iter().all(|p| !p.contains("secret")));
        assert_eq!(check_schema(&json!(5)).len(), 1);
    }

    #[test]
    fn bounds_and_lengths_are_enforced_without_echo() {
        let schema = json!({"type": "object", "properties": {
            "amount": {"type": "integer", "minimum": 1, "maximum": 1000},
            "name": {"type": "string", "minLength": 2, "maxLength": 4}
        }});
        assert!(validate_input(&schema, &json!({"amount": 5, "name": "abc"})).is_empty());
        let low = validate_input(&schema, &json!({"amount": 0, "name": "a"}));
        assert_eq!(low.len(), 2, "{low:?}");
        assert!(low[0].starts_with("/amount: below the minimum"));
        assert!(low[1].starts_with("/name: too short"));
        let high = validate_input(&schema, &json!({"amount": 1001, "name": "abcde"}));
        assert!(high[0].starts_with("/amount: above the maximum"));
        assert!(high[1].starts_with("/name: too long"));
        assert!(check_schema(&schema).is_empty());
        assert_eq!(check_schema(&json!({"minimum": "1"})).len(), 1);
        assert_eq!(check_schema(&json!({"maxLength": -1})).len(), 1);
    }

    #[test]
    fn an_unsupported_keyword_fails_closed() {
        let problems = validate_input(&json!({"pattern": "^a"}), &json!("a"));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("`pattern` is not supported"));
    }
}
