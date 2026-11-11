//! The generated schemas against golden files, and the derive's rules.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;

use common::{NoteController, RenameArgs, TallyArgs};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_controllers_macros::MethodSchema;

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Compares `actual` with the golden file; `UPDATE_GOLDEN=1` rewrites it.
fn assert_golden(name: &str, actual: &Json) {
    let path = golden_path(name);
    let rendered = serde_json::to_string_pretty(actual).expect("renders") + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).expect("writes the golden file");
    }
    let expected = std::fs::read_to_string(&path).expect("the golden file exists");
    assert_eq!(
        rendered, expected,
        "schema changed; review it, then rerun with UPDATE_GOLDEN=1"
    );
}

#[test]
fn the_sample_controllers_schemas_match_the_golden_file() {
    assert_golden("note_schemas.json", &NoteController::whitelisted_schemas());
}

#[test]
fn argument_schemas_list_required_and_optional_fields() {
    let rename = RenameArgs::method_schema();
    assert_eq!(rename["required"], json!(["title"]));
    assert_eq!(rename["additionalProperties"], json!(false));
    let tally = TallyArgs::method_schema();
    assert_eq!(tally["required"], json!(["numbers", "mode"]));
    assert_eq!(
        tally["properties"]["mode"],
        json!({"type": "string", "enum": ["Sum", "largest"]})
    );
}

#[derive(Deserialize, MethodSchema)]
#[method_schema(allow_unknown)]
#[allow(dead_code)]
struct Loose {
    #[serde(rename = "renamed")]
    original: u8,
    #[serde(skip)]
    #[allow(dead_code)]
    hidden: String,
    #[serde(default)]
    flag: bool,
}

#[test]
fn rename_skip_default_and_allow_unknown_are_followed() {
    let schema = Loose::method_schema();
    assert_eq!(schema["additionalProperties"], json!(true));
    assert_eq!(schema["required"], json!(["renamed"]));
    let props = schema["properties"].as_object().unwrap();
    assert!(props.contains_key("renamed"));
    assert!(!props.contains_key("hidden"));
    assert!(!props.contains_key("original"));
}
