//! [`validate_input`]: checks a manual or API start's input against the trigger's JSON schema.
//!
//! The check understands the subset a start form needs: `type`, `required`, `properties`,
//! `additionalProperties` (a boolean), `enum`, `items`, the integer bounds `minimum` and
//! `maximum`, the string lengths `minLength` and `maxLength` (in characters) and `pattern`. A
//! schema keyword outside that subset is reported as a problem rather than ignored, so a schema this crate cannot enforce refuses
//! the start (fail closed) instead of silently accepting any input.
//!
//! `pattern` is matched unanchored, as JSON Schema defines it, with the `regex` crate (linear
//! time: no backtracking, so no pattern can make a start take long). A pattern is compiled with a
//! size limit; one that does not compile or is too large is refused by [`check_schema`] when the
//! definition is published, and again at run time (fail closed) if it ever got here.
//!
//! Every problem is listed, not only the first. A problem names the JSON pointer of the value and
//! the fault kind only; it never echoes the caller's value.

use regex::{Regex, RegexBuilder};
use serde_json::Value as Json;

/// The most compiled program size, in bytes, a pattern may take (also the lazy DFA's budget).
const PATTERN_SIZE_LIMIT: usize = 64 * 1024;

/// The longest pattern text accepted.
const MAX_PATTERN_CHARS: usize = 1_024;

/// The deepest group nesting a pattern may have.
const PATTERN_NEST_LIMIT: u32 = 32;

/// Compiles `pattern` with the bounds above; `None` when it does not compile or is too large.
fn compile(pattern: &str) -> Option<Regex> {
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return None;
    }
    RegexBuilder::new(pattern)
        .size_limit(PATTERN_SIZE_LIMIT)
        .dfa_size_limit(PATTERN_SIZE_LIMIT)
        .nest_limit(PATTERN_NEST_LIMIT)
        .build()
        .ok()
}

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
            "pattern" => {
                if !value.as_str().is_some_and(|text| compile(text).is_some()) {
                    problems.push(format!(
                        "{at}: a schema regular expression is not valid or is too large"
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
                | "pattern"
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
    if let Some(pattern) = rules.get("pattern") {
        match (pattern.as_str().and_then(compile), value) {
            (Some(regex), Json::String(text)) => {
                if !regex.is_match(text) {
                    problems.push(format!("{at}: does not match the pattern"));
                }
            }
            // A pattern only constrains strings.
            (Some(_), _) => {}
            // Refused at publish; here it fails closed.
            (None, _) => problems.push(format!("{at}: the schema `pattern` cannot be enforced")),
        }
    }
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
        assert_eq!(problems.len(), 2, "{problems:?}");
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
        let problems = validate_input(&json!({"format": "email"}), &json!("a"));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("`format` is not supported"));
    }

    #[test]
    fn a_pattern_matches_unanchored_and_anchored() {
        let loose = json!({"type": "string", "pattern": "[0-9]+"});
        assert!(validate_input(&loose, &json!("abc 42 def")).is_empty());
        assert_eq!(validate_input(&loose, &json!("abc")).len(), 1);
        let anchored = json!({"type": "string", "pattern": "^[A-Za-z0-9 .-]+$"});
        assert!(validate_input(&anchored, &json!("Order 12.5-b")).is_empty());
        assert_eq!(validate_input(&anchored, &json!("bad;value")).len(), 1);
        // A pattern constrains strings only.
        assert!(validate_input(&json!({"pattern": "^a$"}), &json!(5)).is_empty());
    }

    #[test]
    fn a_pattern_inside_properties_and_items_is_enforced_with_a_pointer() {
        let schema = json!({"type": "object", "properties": {
            "code": {"type": "string", "pattern": "^[A-Z]{3}$"},
            "tags": {"type": "array", "items": {"type": "string", "pattern": "^t"}}}});
        let problems = validate_input(&schema, &json!({"code": "abcd", "tags": ["t1", "x"]}));
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("/code: does not match"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("/tags/1: does not match"))
        );
    }

    #[test]
    fn an_invalid_or_oversize_pattern_is_refused_at_publish_without_its_text() {
        let marker = "SECRET-PATTERN-MARKER";
        let bad = json!({"type": "object", "properties": {
            "a": {"pattern": format!("({marker}")},
            "b": {"pattern": "x".repeat(MAX_PATTERN_CHARS + 1)},
            "c": {"pattern": r"(a{1000}){1000}"},
            "d": {"items": {"pattern": 5}}}});
        let problems = check_schema(&bad);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems.iter().all(|p| !p.contains(marker)));
        assert!(problems.iter().any(|p| p.starts_with("/a:")));
        assert!(check_schema(&json!({"properties": {"a": {"pattern": "^a$"}}})).is_empty());
    }

    #[test]
    fn a_bad_pattern_that_reaches_run_time_fails_closed_and_never_echoes_the_value() {
        let problems = validate_input(&json!({"pattern": "(unclosed"}), &json!("SECRET-VALUE"));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("cannot be enforced"));
        assert!(!problems[0].contains("SECRET-VALUE") && !problems[0].contains("unclosed"));
        let miss = validate_input(&json!({"pattern": "^a$"}), &json!("SECRET-VALUE"));
        assert!(!miss[0].contains("SECRET-VALUE"));
    }

    #[test]
    fn a_pathological_pattern_does_not_take_long() {
        // Backtracking engines take exponential time here; the regex crate does not.
        let schema = json!({"pattern": "^(a+)+$"});
        let input = json!(format!("{}b", "a".repeat(10_000)));
        assert_eq!(validate_input(&schema, &input).len(), 1);
    }
}
