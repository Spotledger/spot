//! The corpus walker: every `tests/corpus/<case>/` case must transpile to
//! exactly its committed expectation — `expected.js` (byte-equal) for
//! accepted cases, `expected.diagnostics.json` (code + 1-based positions) for
//! rejected ones. Messages are deliberately not recorded: they track oxc's
//! wording and the editor renders them; codes and positions are the contract.
//!
//! Set `SPOT_GOLDEN_WRITE=1` to (re)generate the expectation files from the
//! current implementation; CI never sets it, so drift fails there.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use spot_ts_transpile::{Options, transpile};

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

/// Inputs sorted by case name, so failures and goldens are deterministic.
fn cases() -> BTreeMap<String, String> {
    let mut cases = BTreeMap::new();
    let root = corpus_root();
    for entry in std::fs::read_dir(&root).expect("corpus directory readable") {
        let entry = entry.expect("corpus entry readable");
        let case = entry.path().join("input.ts");
        if entry.path().is_dir() && case.is_file() {
            let name = entry.file_name().into_string().expect("case name utf8");
            cases.insert(
                name,
                std::fs::read_to_string(&case).expect("input readable"),
            );
        }
    }
    cases
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct ExpectedDiagnostic {
    code: String,
    line: u32,
    column: u32,
    end_line: u32,
    end_column: u32,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ExpectedDiagnostics {
    diagnostics: Vec<ExpectedDiagnostic>,
}

fn golden_write_enabled() -> bool {
    std::env::var("SPOT_GOLDEN_WRITE").is_ok()
}

#[test]
fn corpus_cases_match_their_expectations() {
    let cases = cases();
    assert!(
        cases.len() >= 60,
        "the corpus holds at least 60 cases, found {}",
        cases.len()
    );
    for (name, source) in &cases {
        let outcome = transpile(source, "corpus.ts", &Options::default());
        match outcome {
            Ok(transpiled) => {
                let expected = corpus_root().join(name).join("expected.js");
                if golden_write_enabled() {
                    std::fs::write(&expected, &transpiled.js).expect("golden js written");
                    continue;
                }
                let expected = std::fs::read_to_string(&expected)
                    .unwrap_or_else(|error| panic!("case {name} is missing expected.js: {error}"));
                assert_eq!(transpiled.js, expected, "case {name}: JavaScript drifted");
            }
            Err(diagnostics) => {
                let expected = corpus_root().join(name).join("expected.diagnostics.json");
                let recorded: Vec<ExpectedDiagnostic> = diagnostics
                    .iter()
                    .map(|diagnostic| ExpectedDiagnostic {
                        code: diagnostic.code.clone(),
                        line: diagnostic.line,
                        column: diagnostic.column,
                        end_line: diagnostic.end_line,
                        end_column: diagnostic.end_column,
                    })
                    .collect();
                if golden_write_enabled() {
                    let body = ExpectedDiagnostics {
                        diagnostics: recorded,
                    };
                    let json = serde_json::to_string_pretty(&body).expect("golden written");
                    std::fs::write(&expected, format!("{json}\n")).expect("golden written");
                    continue;
                }
                let text = std::fs::read_to_string(&expected).unwrap_or_else(|error| {
                    panic!("case {name} missing expected.diagnostics.json: {error}")
                });
                let expected: ExpectedDiagnostics =
                    serde_json::from_str(&text).unwrap_or_else(|error| {
                        panic!("case {name}: expected file malformed: {error}")
                    });
                assert_eq!(
                    recorded.len(),
                    expected.diagnostics.len(),
                    "case {name}: diagnostic count drifted"
                );
                for (seen, want) in recorded.iter().zip(expected.diagnostics.iter()) {
                    assert_eq!(seen.code, want.code, "case {name}: code drifted");
                    assert_eq!(
                        (seen.line, seen.column, seen.end_line, seen.end_column),
                        (want.line, want.column, want.end_line, want.end_column),
                        "case {name}: position drifted"
                    );
                }
            }
        }
    }
}

#[test]
fn module_records_match_the_emitted_javascript() {
    // The import/export records are what the engine resolves; for the cases
    // whose links survive the type strip they must be exact.
    let cases = cases();

    let named = cases.get("import_named").expect("corpus case");
    let out = transpile(named, "corpus.ts", &Options::default()).expect("accepted");
    assert!(
        out.js
            .contains("import { findMany } from \"spot:documents\";"),
        "the value binding survives the strip: {}",
        out.js
    );
    assert_eq!(out.imports.len(), 1);
    let first = &out.imports[0];
    assert_eq!(first.module, "spot:documents");
    // `Document` is used only as a type, so oxc drops it from the emitted
    // import — and the record must match what the engine will see.
    assert_eq!(
        first.named,
        vec![spot_ts_transpile::ImportedName {
            imported: "findMany".into(),
            local: "findMany".into()
        }]
    );
    assert_eq!(first.default, None);
    assert_eq!(first.namespace, None);
    assert_eq!(out.exports, vec!["first"]);

    // A type-only import disappears from the JavaScript and from the record.
    let type_only = cases.get("import_type_only").expect("corpus case");
    let out = transpile(type_only, "corpus.ts", &Options::default()).expect("accepted");
    assert!(out.js.is_empty() || !out.js.contains("spot:users"));
    assert!(out.imports.is_empty(), "type-only import not reported");

    // The three import shapes land in their fields.
    let namespace = cases.get("import_namespace").expect("corpus case");
    let out = transpile(namespace, "corpus.ts", &Options::default()).expect("accepted");
    assert_eq!(out.imports[0].namespace.as_deref(), Some("meta"));

    let default = cases.get("import_default").expect("corpus case");
    let out = transpile(default, "corpus.ts", &Options::default()).expect("accepted");
    assert_eq!(out.imports[0].default.as_deref(), Some("spot"));

    let aliased = cases.get("import_alias").expect("corpus case");
    let out = transpile(aliased, "corpus.ts", &Options::default()).expect("accepted");
    assert_eq!(out.imports[0].named[0].imported, "findMany");
    assert_eq!(out.imports[0].named[0].local, "list");

    // Two value-used named bindings survive together.
    let two = cases.get("import_two_values").expect("corpus case");
    let out = transpile(two, "corpus.ts", &Options::default()).expect("accepted");
    assert_eq!(out.imports.len(), 1);
    assert_eq!(
        out.imports[0].named,
        vec![
            spot_ts_transpile::ImportedName {
                imported: "audit".into(),
                local: "audit".into()
            },
            spot_ts_transpile::ImportedName {
                imported: "emit".into(),
                local: "emit".into()
            },
        ]
    );

    // Declared exports (const/function/class/enum) are all recorded.
    let multi = transpile(
        cases.get("export_named_list").expect("corpus case"),
        "corpus.ts",
        &Options::default(),
    )
    .expect("accepted");
    assert_eq!(multi.exports, vec!["alpha", "beta"]);

    let enumeration = transpile(
        cases.get("enum_color").expect("corpus case"),
        "corpus.ts",
        &Options::default(),
    )
    .expect("accepted");
    assert_eq!(enumeration.exports, vec!["Color", "label"]);

    let default_fn = transpile(
        cases.get("export_default_function").expect("corpus case"),
        "corpus.ts",
        &Options::default(),
    )
    .expect("accepted");
    assert_eq!(default_fn.exports, vec!["default"]);

    // Destructuring exports bind patterns' names.
    let pattern = transpile(
        cases.get("export_object_pattern").expect("corpus case"),
        "corpus.ts",
        &Options::default(),
    )
    .expect("accepted");
    assert_eq!(pattern.exports, vec!["width", "height"]);
}
