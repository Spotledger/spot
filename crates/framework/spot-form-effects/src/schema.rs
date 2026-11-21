//! The protocol's JSON Schema, generated from the types and checked in at
//! `schema/effects.v1.json`.
//!
//! The schema is the contract the C# client (WP-UI-12.T2) is written against and the shape a
//! future `contracts/` promotion takes. [`effects_v1_schema`] builds it; the checked-in file must
//! equal its pretty form exactly, which [`tests::the_checked_in_schema_is_current`] proves. When
//! the types change, run `cargo test -p spot-form-effects -- --ignored regenerate_the_schema`.

use serde_json::{Value as Json, json};

use crate::effect::PROTOCOL_VERSION;

/// The path to the checked-in schema, from the crate root.
pub const SCHEMA_PATH: &str = "schema/effects.v1.json";

/// The schema document, in memory.
#[must_use]
pub fn effects_v1_schema() -> Json {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://spotledger.example/schema/effects.v1.json",
        "title": "Spotledger UI effects protocol v1",
        "description": "The declarative effects a server-side form script may ask a client to apply. Additive within v1: a client ignores an unknown kind or member; a client that does not know `version` applies nothing.",
        "type": "object",
        "required": ["version", "effects"],
        "additionalProperties": false,
        "properties": {
            "version": {
                "const": PROTOCOL_VERSION,
                "description": "The protocol version. Only version 1 is defined."
            },
            "effects": {
                "type": "array",
                "description": "The effects, in application order.",
                "items": { "$ref": "#/$defs/effect" }
            }
        },
        "$defs": {
            "path": {
                "type": "string",
                "description": "A field path: `field` or `table[idx].field` with idx from 1.",
                "pattern": "^[^\\[\\].\\s]+(\\[[1-9][0-9]*\\]\\.[^\\[\\].\\s]+)?$"
            },
            "message_level": {
                "type": "string",
                "enum": ["info", "warning", "error"]
            },
            "option": {
                "type": "object",
                "required": ["value"],
                "additionalProperties": false,
                "properties": {
                    "value": { "type": "string" },
                    "label": { "type": "string" }
                }
            },
            "effect": {
                "description": "One effect. A client that meets a kind it does not know skips it.",
                "oneOf": [
                    { "$ref": "#/$defs/set_value" },
                    { "$ref": "#/$defs/set_read_only" },
                    { "$ref": "#/$defs/set_hidden" },
                    { "$ref": "#/$defs/set_required" },
                    { "$ref": "#/$defs/set_options" },
                    { "$ref": "#/$defs/set_label" },
                    { "$ref": "#/$defs/add_action" },
                    { "$ref": "#/$defs/remove_action" },
                    { "$ref": "#/$defs/show_message" },
                    { "$ref": "#/$defs/open_dialog" },
                    { "$ref": "#/$defs/focus" },
                    { "$ref": "#/$defs/set_filter" },
                    { "$ref": "#/$defs/batch" }
                ]
            },
            "set_value": {
                "type": "object",
                "required": ["kind", "path", "value"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "set_value" },
                    "path": { "$ref": "#/$defs/path" },
                    "value": {
                        "description": "The field's wire value; a decimal is its exact string."
                    }
                }
            },
            "set_read_only": {
                "type": "object",
                "required": ["kind", "path", "read_only"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "set_read_only" },
                    "path": { "$ref": "#/$defs/path" },
                    "read_only": { "type": "boolean" }
                }
            },
            "set_hidden": {
                "type": "object",
                "required": ["kind", "path", "hidden"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "set_hidden" },
                    "path": { "$ref": "#/$defs/path" },
                    "hidden": { "type": "boolean" }
                }
            },
            "set_required": {
                "type": "object",
                "required": ["kind", "path", "required"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "set_required" },
                    "path": { "$ref": "#/$defs/path" },
                    "required": { "type": "boolean" }
                }
            },
            "set_options": {
                "type": "object",
                "required": ["kind", "path", "options"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "set_options" },
                    "path": { "$ref": "#/$defs/path" },
                    "options": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/option" }
                    }
                }
            },
            "set_label": {
                "type": "object",
                "required": ["kind", "path", "label"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "set_label" },
                    "path": { "$ref": "#/$defs/path" },
                    "label": { "type": "string", "minLength": 1 }
                }
            },
            "add_action": {
                "type": "object",
                "required": ["kind", "id", "label", "effect"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "add_action" },
                    "id": { "type": "string", "pattern": "^[a-z][a-z0-9_.:-]{0,63}$" },
                    "label": { "type": "string", "minLength": 1 },
                    "effect": { "$ref": "#/$defs/effect" }
                }
            },
            "remove_action": {
                "type": "object",
                "required": ["kind", "id"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "remove_action" },
                    "id": { "type": "string", "pattern": "^[a-z][a-z0-9_.:-]{0,63}$" }
                }
            },
            "show_message": {
                "type": "object",
                "required": ["kind", "level"],
                "additionalProperties": false,
                "anyOf": [
                    { "required": ["text"] },
                    { "required": ["text_key"] }
                ],
                "properties": {
                    "kind": { "const": "show_message" },
                    "level": { "$ref": "#/$defs/message_level" },
                    "text": { "type": "string" },
                    "text_key": { "type": "string" },
                    "field": { "$ref": "#/$defs/path" }
                }
            },
            "open_dialog": {
                "type": "object",
                "required": ["kind", "dialog_id"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "open_dialog" },
                    "dialog_id": { "type": "string", "pattern": "^[a-z][a-z0-9_.:-]{0,63}$" },
                    "params": { "type": "object" }
                }
            },
            "focus": {
                "type": "object",
                "required": ["kind", "path"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "focus" },
                    "path": { "$ref": "#/$defs/path" }
                }
            },
            "set_filter": {
                "type": "object",
                "required": ["kind", "link_path", "filter"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "set_filter" },
                    "link_path": { "$ref": "#/$defs/path" },
                    "filter": {
                        "type": "string",
                        "description": "A shared-expression-language expression over `doc`."
                    }
                }
            },
            "batch": {
                "type": "object",
                "required": ["kind", "effects"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "const": "batch" },
                    "effects": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/effect" }
                    }
                }
            }
        }
    })
}

/// The schema as the checked-in file holds it: pretty JSON with a final newline.
#[must_use]
pub fn effects_v1_schema_text() -> String {
    let mut text =
        serde_json::to_string_pretty(&effects_v1_schema()).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::effect::Effect;
    use std::path::PathBuf;

    fn checked_in() -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SCHEMA_PATH);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn the_checked_in_schema_is_current() {
        assert_eq!(
            checked_in(),
            effects_v1_schema_text(),
            "the schema is generated; regenerate it with `cargo test -p spot-form-effects -- --ignored regenerate_the_schema`"
        );
    }

    #[test]
    #[ignore = "rewrites the checked-in schema"]
    fn regenerate_the_schema() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SCHEMA_PATH);
        std::fs::write(&path, effects_v1_schema_text()).expect("the schema is writable");
    }

    #[test]
    fn every_effect_kind_has_its_definition_and_const() {
        let schema = effects_v1_schema();
        let defs = schema["$defs"].as_object().unwrap();
        let one_of = defs["effect"]["oneOf"].as_array().unwrap();
        assert_eq!(one_of.len(), Effect::ALL_KINDS.len());
        for kind in Effect::ALL_KINDS {
            let def = defs
                .get(kind)
                .unwrap_or_else(|| panic!("no definition for {kind}"));
            assert_eq!(def["properties"]["kind"]["const"], json!(kind));
            assert!(
                one_of
                    .iter()
                    .any(|r| r["$ref"] == json!(format!("#/$defs/{kind}"))),
                "{kind} is not in `effect.oneOf`"
            );
        }
        assert_eq!(
            defs["batch"]["properties"]["effects"]["items"]["$ref"],
            json!("#/$defs/effect")
        );
    }
}
