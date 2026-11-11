//! [`validate_input`]: checks a manual or API start's input against the trigger's JSON schema.
//!
//! The check understands the subset a start form needs: `type`, `required`, `properties`,
//! `additionalProperties` (a boolean), `enum` and `items`. A schema keyword outside that subset
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
            "type" | "required" | "properties" | "additionalProperties" | "enum" | "items"
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
    fn an_unsupported_keyword_fails_closed() {
        let problems = validate_input(&json!({"pattern": "^a"}), &json!("a"));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("`pattern` is not supported"));
    }
}
