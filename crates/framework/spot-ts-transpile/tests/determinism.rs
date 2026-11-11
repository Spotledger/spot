//! Determinism: the acceptance-critical property — identical input gives
//! byte-identical `js` and `source_map`, across calls and options copies.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use spot_ts_transpile::{Options, transpile};

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

#[test]
fn every_accepted_case_is_byte_identical_across_runs() {
    for entry in std::fs::read_dir(corpus_root()).expect("corpus readable") {
        let path = entry.expect("entry readable").path();
        if !path.is_dir() || !path.join("expected.js").is_file() {
            continue;
        }
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let source = std::fs::read_to_string(path.join("input.ts")).expect("input readable");
        let first = transpile(&source, "corpus.ts", &Options::default()).expect("accepted case");
        let second = transpile(&source, "corpus.ts", &Options::default()).expect("accepted case");
        assert_eq!(first.js, second.js, "case {name}: js differs across runs");
        assert_eq!(
            first.source_map, second.source_map,
            "case {name}: map differs"
        );
        assert!(
            !first.js.contains("corpus.ts\r"),
            "case {name}: line endings leaked"
        );
        // No timestamps in the output shape at all.
        assert!(
            !first.source_map.contains("timestamp"),
            "case {name}: map has timestamps"
        );
    }
}

#[test]
fn file_name_reduces_absolute_paths_in_the_map() {
    let source = "export const one = 1;\n";
    let absolute =
        transpile(source, "/tmp/never-on-disk/script.ts", &Options::default()).expect("accepted");
    let map: serde_json::Value = serde_json::from_str(&absolute.source_map).expect("map json");
    let sources = map["sources"].as_array().expect("sources array");
    assert_eq!(sources.len(), 1);
    assert_eq!(
        sources[0].as_str(),
        Some("script.ts"),
        "absolute path stripped to file name"
    );

    // A relative file passes through untouched.
    let relative = transpile(source, "hooks/on-save.ts", &Options::default()).expect("accepted");
    let map: serde_json::Value = serde_json::from_str(&relative.source_map).expect("map json");
    assert_eq!(map["sources"][0].as_str(), Some("hooks/on-save.ts"));
}

#[test]
fn map_carries_the_input_version_three_fields() {
    let source = "export const add = (a: number, b: number): number => a + b;\n";
    let out = transpile(source, "add.ts", &Options::default()).expect("accepted");
    let map: serde_json::Value = serde_json::from_str(&out.source_map).expect("map json");
    assert_eq!(map["version"].as_i64(), Some(3));
    assert!(
        map["mappings"]
            .as_str()
            .is_some_and(|mappings| !mappings.is_empty())
    );
    let contents = map["sourcesContent"].as_array().expect("contents");
    assert_eq!(
        contents[0].as_str(),
        Some(source),
        "sourcesContent holds the input"
    );
}
