//! The argument-schema keywords: `additionalProperties`, `enum`, the length, integer and item
//! bounds, the schema-shape check, and the rule that no error repeats an argument value.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value as Json, json};
use spot_functions::{MAX_UNIQUE_ITEMS, SchemaError, check_schema, validate_arguments};

/// A value no schema fact contains, put into arguments to prove errors never repeat values.
const SECRET: &str = "S3CR3T-VALUE-4711";

fn ok(schema: &Json, args: &Json) {
    if let Err(error) = validate_arguments(schema, args) {
        panic!("expected {args} to pass {schema}, got {error}");
    }
}

fn refused(schema: &Json, args: &Json) -> SchemaError {
    match validate_arguments(schema, args) {
        Ok(()) => panic!("expected {args} to be refused by {schema}"),
        Err(error) => error,
    }
}

// ---------------------------------------------------------------------------------------------
// additionalProperties

#[test]
fn additional_properties_false_refuses_an_unlisted_property() {
    let schema = json!({
        "type": "object",
        "required": ["query"],
        "additionalProperties": false,
        "properties": { "query": { "type": "string" } }
    });
    ok(&schema, &json!({ "query": "x" }));
    let error = refused(&schema, &json!({ "query": "x", "tier": 3 }));
    assert_eq!(error.path, "tier");
    assert_eq!(error.message, "unexpected property");
    assert_eq!(error.to_string(), "tier: unexpected property");
}

#[test]
fn additional_properties_true_or_absent_keeps_unlisted_properties() {
    let open = json!({ "type": "object", "additionalProperties": true, "properties": {} });
    ok(&open, &json!({ "anything": 1 }));
    let absent = json!({ "type": "object", "properties": { "a": { "type": "integer" } } });
    ok(&absent, &json!({ "a": 1, "b": "free" }));
}

#[test]
fn additional_properties_false_without_properties_refuses_every_property() {
    let schema = json!({ "type": "object", "additionalProperties": false });
    ok(&schema, &json!({}));
    assert_eq!(refused(&schema, &json!({ "a": 1 })).path, "a");
}

#[test]
fn additional_properties_false_applies_in_a_nested_object() {
    let schema = json!({
        "type": "object",
        "properties": {
            "approval": {
                "type": "object",
                "additionalProperties": false,
                "required": ["approver"],
                "properties": { "approver": { "type": "string" } }
            }
        }
    });
    ok(
        &schema,
        &json!({ "approval": { "approver": "a" }, "outer": 1 }),
    );
    let error = refused(
        &schema,
        &json!({ "approval": { "approver": "a", "extra": 1 } }),
    );
    assert_eq!(error.path, "approval.extra");
}

#[test]
fn additional_properties_false_applies_to_array_items() {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "qty": { "type": "integer" } }
                }
            }
        }
    });
    ok(
        &schema,
        &json!({ "items": [{ "qty": 1 }, { "qty": 2 }, {}] }),
    );
    let error = refused(
        &schema,
        &json!({ "items": [{ "qty": 1 }, { "qty": 2 }, { "qty": 3, "extra": true }] }),
    );
    assert_eq!(error.path, "items.2.extra");
}

#[test]
fn additional_properties_as_a_subschema_validates_every_unlisted_property() {
    let schema = json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "additionalProperties": { "type": "integer", "minimum": 0 }
    });
    ok(&schema, &json!({ "name": "n", "a": 1, "b": 0 }));
    // A listed property is judged by its own schema, not the subschema.
    ok(&schema, &json!({ "name": "text" }));
    let wrong_type = refused(&schema, &json!({ "name": "n", "a": "one" }));
    assert_eq!(wrong_type.path, "a");
    assert_eq!(wrong_type.message, "expected integer");
    let out_of_range = refused(&schema, &json!({ "b": -1 }));
    assert_eq!(out_of_range.path, "b");
}

#[test]
fn a_long_unexpected_property_name_is_cut_in_the_error() {
    let schema = json!({ "type": "object", "additionalProperties": false });
    let name = "n".repeat(500);
    let mut args = serde_json::Map::new();
    args.insert(name.clone(), json!(1));
    let error = refused(&schema, &Json::Object(args));
    assert!(error.path.len() < 100, "{}", error.path.len());
    assert!(!error.to_string().contains(&name));
}

#[test]
fn additional_properties_does_not_apply_to_a_non_object() {
    // The type check fails first; with no `type`, an array is not an object at all.
    let schema = json!({ "additionalProperties": false });
    ok(&schema, &json!([1, 2]));
    ok(&schema, &json!("text"));
}

// ---------------------------------------------------------------------------------------------
// enum

#[test]
fn enum_accepts_listed_values_and_refuses_others() {
    let schema = json!({ "type": "string", "enum": ["episode", "procedure", "fact"] });
    ok(&schema, &json!("fact"));
    let error = refused(&schema, &json!("other"));
    assert_eq!(error.message, "not one of the allowed values");
    assert!(!error.to_string().contains("other"));

    let mixed = json!({ "enum": [1, "a", null, true] });
    for good in [json!(1), json!("a"), json!(null), json!(true)] {
        ok(&mixed, &good);
    }
    for bad in [json!(2), json!("b"), json!(false), json!("1")] {
        refused(&mixed, &bad);
    }
}

#[test]
fn enum_on_array_items_names_the_item() {
    let schema = json!({
        "type": "object",
        "properties": { "kinds": { "type": "array", "items": { "type": "string", "enum": ["a", "b"] } } }
    });
    ok(&schema, &json!({ "kinds": ["a", "b"] }));
    assert_eq!(
        refused(&schema, &json!({ "kinds": ["a", "z"] })).path,
        "kinds.1"
    );
}

// ---------------------------------------------------------------------------------------------
// minLength / maxLength

#[test]
fn string_length_counts_unicode_scalar_values() {
    let schema = json!({ "type": "string", "minLength": 2, "maxLength": 4 });
    ok(&schema, &json!("ab"));
    ok(&schema, &json!("abcd"));
    // Four scalar values, far more than four bytes.
    ok(&schema, &json!("\u{e9}\u{e9}\u{e9}\u{e9}"));
    ok(&schema, &json!("\u{1F600}\u{1F600}"));
    assert!(
        refused(&schema, &json!("a"))
            .message
            .starts_with("shorter than 2")
    );
    assert!(
        refused(&schema, &json!("abcde"))
            .message
            .starts_with("longer than 4")
    );
    assert!(
        refused(
            &schema,
            &json!("\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}")
        )
        .path
        .is_empty()
    );
}

#[test]
fn string_length_bounds_ignore_other_types() {
    // The type keyword is what refuses a wrong type; a bound alone does not.
    let schema = json!({ "minLength": 3 });
    ok(&schema, &json!(1));
    ok(&schema, &json!(null));
}

// ---------------------------------------------------------------------------------------------
// minimum / maximum

#[test]
fn integer_bounds_are_inclusive() {
    let schema = json!({ "type": "integer", "minimum": 1, "maximum": 10 });
    ok(&schema, &json!(1));
    ok(&schema, &json!(10));
    assert_eq!(
        refused(&schema, &json!(0)).message,
        "below the minimum of 1"
    );
    assert_eq!(
        refused(&schema, &json!(11)).message,
        "above the maximum of 10"
    );
    assert_eq!(
        refused(&schema, &json!(-5)).message,
        "below the minimum of 1"
    );
}

#[test]
fn integer_bounds_hold_beyond_i64() {
    let schema = json!({ "type": "integer", "maximum": 9_223_372_036_854_775_807_u64 });
    ok(&schema, &json!(9_223_372_036_854_775_807_u64));
    refused(&schema, &json!(9_223_372_036_854_775_808_u64));
    let negative = json!({ "minimum": -9_223_372_036_854_775_808_i64 });
    ok(&negative, &json!(-9_223_372_036_854_775_808_i64));
}

#[test]
fn a_fractional_number_under_a_bounded_schema_is_refused() {
    let schema = json!({ "type": "number", "minimum": 0 });
    ok(&schema, &json!(3));
    assert_eq!(refused(&schema, &json!(2.5)).message, "expected an integer");
    // Without a bound a fractional number is still a number.
    ok(&json!({ "type": "number" }), &json!(2.5));
}

#[test]
fn bounds_ignore_non_numbers() {
    let schema = json!({ "minimum": 5, "maximum": 6 });
    ok(&schema, &json!("text"));
    ok(&schema, &json!(null));
}

// ---------------------------------------------------------------------------------------------
// minItems / maxItems

#[test]
fn item_count_bounds_are_inclusive() {
    let schema = json!({ "type": "array", "minItems": 1, "maxItems": 3 });
    ok(&schema, &json!([1]));
    ok(&schema, &json!([1, 2, 3]));
    assert_eq!(refused(&schema, &json!([])).message, "fewer than 1 items");
    assert_eq!(
        refused(&schema, &json!([1, 2, 3, 4])).message,
        "more than 3 items"
    );
}

#[test]
fn item_count_bounds_apply_inside_properties() {
    let schema = json!({
        "type": "object",
        "properties": { "doctypes": { "type": "array", "maxItems": 2, "items": { "type": "string" } } }
    });
    ok(&schema, &json!({ "doctypes": ["a", "b"] }));
    assert_eq!(
        refused(&schema, &json!({ "doctypes": ["a", "b", "c"] })).path,
        "doctypes"
    );
}

// ---------------------------------------------------------------------------------------------
// The existing keywords keep their behaviour

#[test]
fn type_required_and_properties_are_unchanged() {
    let schema = json!({
        "type": "object",
        "required": ["n"],
        "properties": { "n": { "type": "integer" } }
    });
    ok(&schema, &json!({ "n": 1, "free": "form" }));
    assert_eq!(
        refused(&schema, &json!({ "n": "x" })).message,
        "expected integer"
    );
    assert_eq!(
        refused(&schema, &json!({})).message,
        "missing required field \"n\""
    );
    assert_eq!(refused(&schema, &json!([])).message, "expected object");
}

#[test]
fn keywords_the_check_does_not_implement_pass_through() {
    let schema = json!({
        "type": "string",
        "format": "date",
        "pattern": "^x$",
        "description": "free text",
        "default": "d"
    });
    ok(&schema, &json!("anything"));
    // Documented, not enforced: none of these refuses a value that does not satisfy it.
    let unenforced = json!({
        "type": "integer",
        "multipleOf": 7,
        "exclusiveMinimum": 100,
        "oneOf": [{ "type": "string" }],
        "anyOf": [{ "type": "string" }]
    });
    ok(&unenforced, &json!(3));
}

// ---------------------------------------------------------------------------------------------
// uniqueItems

#[test]
fn unique_items_refuses_a_duplicate_scalar_with_the_later_index() {
    let schema = json!({ "type": "array", "uniqueItems": true });
    ok(&schema, &json!([]));
    ok(&schema, &json!(["a"]));
    ok(&schema, &json!(["a", "b", "c"]));
    let error = refused(&schema, &json!(["a", "b", "a"]));
    assert_eq!(error.path, ".2");
    assert_eq!(error.message, "duplicate item");
    let error = refused(&schema, &json!([1, 2, 3, 3]));
    assert_eq!(error.path, ".3");
    refused(&schema, &json!([null, null]));
    refused(&schema, &json!([true, false, true]));
}

#[test]
fn unique_items_compares_types_strictly() {
    let schema = json!({ "type": "array", "uniqueItems": true });
    ok(&schema, &json!([1, "1", true, null]));
    ok(&schema, &json!([0, false, "", null]));
    ok(&schema, &json!([[1], 1, { "a": 1 }]));
}

#[test]
fn unique_items_compares_objects_regardless_of_key_order() {
    let schema = json!({ "type": "array", "uniqueItems": true });
    ok(&schema, &json!([{ "a": 1, "b": 2 }, { "a": 1, "b": 3 }]));
    let error = refused(&schema, &json!([{ "a": 1, "b": 2 }, { "b": 2, "a": 1 }]));
    assert_eq!(error.path, ".1");
    ok(&schema, &json!([{ "a": 1 }, { "a": 1, "b": null }]));
}

#[test]
fn unique_items_compares_arrays_by_order_and_content() {
    let schema = json!({ "type": "array", "uniqueItems": true });
    ok(&schema, &json!([[1, 2], [2, 1]]));
    let error = refused(&schema, &json!([[1, 2], [3], [1, 2]]));
    assert_eq!(error.path, ".2");
    refused(
        &schema,
        &json!([{ "k": [1, { "z": 1, "y": 2 }] }, { "k": [1, { "y": 2, "z": 1 }] }]),
    );
}

#[test]
fn unique_items_names_the_later_item_in_a_nested_path() {
    let schema = json!({
        "type": "object",
        "properties": {
            "kinds": { "type": "array", "uniqueItems": true, "items": { "type": "string" } }
        }
    });
    ok(&schema, &json!({ "kinds": ["episode", "fact"] }));
    let error = refused(&schema, &json!({ "kinds": ["fact", "episode", "fact"] }));
    assert_eq!(error.path, "kinds.2");
    assert_eq!(error.message, "duplicate item");
    assert_eq!(error.to_string(), "kinds.2: duplicate item");
}

#[test]
fn unique_items_false_or_absent_keeps_duplicates() {
    ok(
        &json!({ "type": "array", "uniqueItems": false }),
        &json!([1, 1, 1]),
    );
    ok(&json!({ "type": "array" }), &json!(["a", "a"]));
    ok(&json!({ "uniqueItems": true }), &json!("not an array"));
    ok(&json!({ "uniqueItems": true }), &json!({ "a": 1, "b": 1 }));
}

#[test]
fn unique_items_never_echoes_a_value() {
    let schema = json!({ "type": "array", "uniqueItems": true });
    let error = refused(&schema, &json!([SECRET, SECRET]));
    let text = format!("{error} {error:?}");
    assert!(!text.contains(SECRET), "{text}");
    let nested = json!({ "properties": { "l": { "uniqueItems": true } } });
    let error = refused(&nested, &json!({ "l": [{ "k": SECRET }, { "k": SECRET }] }));
    let text = format!("{error} {error:?}");
    assert!(!text.contains(SECRET), "{text}");
}

#[test]
fn unique_items_is_bounded_by_a_cap() {
    let schema = json!({ "type": "array", "uniqueItems": true });
    let at_cap: Vec<Json> = (0..MAX_UNIQUE_ITEMS).map(|n| json!(n)).collect();
    ok(&schema, &Json::Array(at_cap.clone()));
    let mut over = at_cap;
    over.push(json!(MAX_UNIQUE_ITEMS));
    let error = refused(&schema, &Json::Array(over));
    assert_eq!(error.path, "");
    assert!(error.message.contains("1024"), "{}", error.message);
    // Without uniqueItems the same long array is fine.
    let long: Vec<Json> = (0..=MAX_UNIQUE_ITEMS).map(|n| json!(n)).collect();
    ok(&json!({ "type": "array" }), &Json::Array(long));
}

#[test]
fn max_items_refuses_before_the_uniqueness_walk() {
    let schema = json!({ "type": "array", "maxItems": 2, "uniqueItems": true });
    let error = refused(&schema, &json!([1, 1, 1]));
    assert_eq!(error.message, "more than 2 items");
}

#[test]
fn the_recall_memory_schema_checks_and_refuses_a_duplicate_kind() {
    // Copied from `spot-cli/src/services/agent_memory/tools.rs` (spot-functions must not
    // depend on spot-cli), with the bounds the tool computes from its configuration filled in.
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["query"],
        "properties": {
            "query": { "type": "string", "minLength": 1, "maxLength": 2000 },
            "k": { "type": "integer", "minimum": 1, "maximum": 20, "default": 5 },
            "kinds": {
                "type": "array",
                "minItems": 1,
                "maxItems": 3,
                "uniqueItems": true,
                "items": { "type": "string", "enum": ["episode", "procedure", "fact"] }
            }
        }
    });
    assert_eq!(check_schema(&schema), Ok(()));
    ok(
        &schema,
        &json!({ "query": "q", "kinds": ["episode", "fact"] }),
    );
    ok(&schema, &json!({ "query": "q" }));
    let error = refused(&schema, &json!({ "query": "q", "kinds": ["fact", "fact"] }));
    assert_eq!(error.path, "kinds.1");
    assert_eq!(error.message, "duplicate item");
    // The other keywords of the same schema still apply.
    assert_eq!(
        refused(&schema, &json!({ "query": "q", "kinds": ["other"] })).path,
        "kinds.0"
    );
    assert_eq!(
        refused(&schema, &json!({ "query": "q", "extra": 1 })).message,
        "unexpected property"
    );
}

// ---------------------------------------------------------------------------------------------
// The schema-shape check

#[test]
fn check_schema_accepts_every_implemented_keyword() {
    let schema = json!({
        "type": "object",
        "required": ["a"],
        "additionalProperties": false,
        "properties": {
            "a": { "type": "integer", "minimum": -3, "maximum": 3 },
            "s": { "type": "string", "enum": ["x", "y"], "minLength": 1, "maxLength": 9 },
            "l": {
                "type": "array",
                "minItems": 0,
                "maxItems": 2,
                "uniqueItems": true,
                "items": { "type": "string" }
            },
            "m": { "type": "object", "additionalProperties": { "type": "integer" } },
            "free": { "format": "date", "description": "ignored" }
        }
    });
    assert_eq!(check_schema(&schema), Ok(()));
    assert_eq!(check_schema(&json!({})), Ok(()));
}

#[test]
fn check_schema_refuses_malformed_nodes_with_their_path() {
    let cases = [
        (json!([]), "", "a schema node must be a JSON object"),
        (json!({ "type": 3 }), "", "`type` must be a string"),
        (
            json!({ "type": "decimal" }),
            "",
            "unknown schema type \"decimal\"",
        ),
        (
            json!({ "required": "a" }),
            "",
            "`required` must be an array of property names",
        ),
        (
            json!({ "required": [1] }),
            "",
            "`required` entries must be strings",
        ),
        (
            json!({ "properties": [] }),
            "",
            "`properties` must be a JSON object",
        ),
        (
            json!({ "properties": { "a": 1 } }),
            "a",
            "a schema node must be a JSON object",
        ),
        (
            json!({ "additionalProperties": 1 }),
            "",
            "`additionalProperties` must be a boolean or a schema",
        ),
        (
            json!({ "additionalProperties": { "type": "nope" } }),
            "*",
            "unknown schema type \"nope\"",
        ),
        (
            json!({ "items": [] }),
            "*",
            "a schema node must be a JSON object",
        ),
        (
            json!({ "enum": [] }),
            "",
            "`enum` must be a non-empty array",
        ),
        (
            json!({ "enum": "a" }),
            "",
            "`enum` must be a non-empty array",
        ),
        (
            json!({ "enum": [{ "a": 1 }] }),
            "",
            "`enum` members must be null, boolean, string or integer values",
        ),
        (
            json!({ "enum": [1.5] }),
            "",
            "`enum` members must be null, boolean, string or integer values",
        ),
        (
            json!({ "minLength": -1 }),
            "",
            "`minLength` must be a non-negative integer",
        ),
        (
            json!({ "maxLength": 2.5 }),
            "",
            "`maxLength` must be a non-negative integer",
        ),
        (
            json!({ "minItems": "1" }),
            "",
            "`minItems` must be a non-negative integer",
        ),
        (
            json!({ "minimum": 1.5 }),
            "",
            "`minimum` must be an integer",
        ),
        (
            json!({ "maximum": "9" }),
            "",
            "`maximum` must be an integer",
        ),
        (
            json!({ "minLength": 5, "maxLength": 2 }),
            "",
            "`minLength` exceeds `maxLength`",
        ),
        (
            json!({ "minItems": 5, "maxItems": 2 }),
            "",
            "`minItems` exceeds `maxItems`",
        ),
        (
            json!({ "minimum": 5, "maximum": 2 }),
            "",
            "`minimum` exceeds `maximum`",
        ),
        (
            json!({ "properties": { "a": { "properties": { "b": { "minimum": 0.5 } } } } }),
            "a.b",
            "`minimum` must be an integer",
        ),
        (
            json!({ "uniqueItems": "yes" }),
            "",
            "`uniqueItems` must be a boolean",
        ),
        (
            json!({ "uniqueItems": 1 }),
            "",
            "`uniqueItems` must be a boolean",
        ),
        (
            json!({ "properties": { "tags": { "type": "array", "uniqueItems": null } } }),
            "tags",
            "`uniqueItems` must be a boolean",
        ),
    ];
    for (schema, path, message) in cases {
        let error = check_schema(&schema).expect_err(&schema.to_string());
        assert_eq!(error.path, path, "{schema}");
        assert_eq!(error.message, message, "{schema}");
    }
}

#[test]
fn a_malformed_keyword_also_fails_at_call_time() {
    // The two checks read the keywords through the same helpers, so a schema that
    // `check_schema` refuses cannot be silently skipped by `validate_arguments` when the walk
    // reaches the node.
    for schema in [
        json!({ "additionalProperties": 1 }),
        json!({ "enum": [] }),
        json!({ "minLength": -1 }),
        json!({ "minimum": 1.5 }),
        json!({ "minItems": 3, "maxItems": 1 }),
        json!({ "uniqueItems": "yes" }),
    ] {
        assert!(check_schema(&schema).is_err(), "{schema}");
        assert!(validate_arguments(&schema, &json!({})).is_err(), "{schema}");
    }
}

// ---------------------------------------------------------------------------------------------
// No error repeats an argument value

#[test]
fn no_error_contains_an_argument_value() {
    // Every refusal below is triggered by a value that carries SECRET (or a property whose
    // *value* does); none of the messages or paths may contain it.
    let strict = json!({
        "type": "object",
        "required": ["code"],
        "additionalProperties": false,
        "properties": {
            "code": { "type": "string", "enum": ["a", "b"], "minLength": 1, "maxLength": 3 },
            "n": { "type": "integer", "minimum": 0, "maximum": 5 },
            "list": { "type": "array", "maxItems": 1, "items": { "type": "string", "maxLength": 3 } },
            "nested": { "type": "object", "additionalProperties": { "type": "integer" } }
        }
    });
    let attempts = [
        json!({ "code": SECRET }),
        json!({ "code": "a", "n": SECRET }),
        json!({ "code": "a", "n": 99 }),
        json!({ "code": "a", "list": [SECRET] }),
        json!({ "code": "a", "list": ["a", SECRET] }),
        json!({ "code": "a", "nested": { "k": SECRET } }),
        json!({ "code": "a", "unlisted": SECRET }),
        json!({ "code": SECRET.len() }),
        json!(SECRET),
        json!([SECRET]),
        json!({ "n": SECRET }),
    ];
    for args in attempts {
        let error = refused(&strict, &args);
        let text = format!("{error} {error:?}");
        assert!(!text.contains(SECRET), "{args} leaked in {text}");
    }
}

#[test]
fn a_property_name_is_the_only_caller_text_an_error_may_carry() {
    let schema = json!({ "type": "object", "additionalProperties": false });
    let error = refused(&schema, &json!({ "bad_name": SECRET }));
    assert_eq!(error.path, "bad_name");
    assert!(!error.to_string().contains(SECRET));
}
