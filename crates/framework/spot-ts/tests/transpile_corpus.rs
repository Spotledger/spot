//! The transpiler and the engine agree (WP-EXT-08.T3).
//!
//! What this test pins, so the engine's and the transpiler's docs can link here:
//!
//! - every accepted program of the `spot-ts-transpile` corpus whose emitted JavaScript makes no
//!   import compiles in the QuickJS engine as an ES module, from bytecode and from source; one
//!   that does import is refused by the engine with `script.compile` ("imports are not allowed"),
//!   by design, and a rejected program stays rejected by the transpiler;
//! - `quickjs_runs_class_fields_the_transpiler_emits`: class fields, `#private` fields and static
//!   fields run as written;
//! - `parameter_properties_become_assigned_fields`;
//! - `abstract_methods_are_absent_from_the_emitted_class`: a subclass that provides the method
//!   works, calling the abstract one on a base instance is a `TypeError`;
//! - `enum_iife_with_pure_annotation_runs`: the `/* @__PURE__ */` enum IIFE evaluates, with reverse
//!   mapping, string enums and inlined `const enum`;
//! - `eval_and_new_function_are_refused_by_the_transpiler_and_the_engine_has_no_eval`;
//! - `a_module_with_an_import_of_anything_but_itself_is_refused_by_the_engine`;
//! - an exception thrown by emitted code is reported at the TypeScript line through the source map;
//!   a thrown `{ code, message, fields }` is `ScriptError::Validation` with everything intact;
//! - decorators and namespaces are rejected by the transpiler (they are corpus cases `r_*`).
//!
//! The run specs are `tests/corpus_run/*.json`; their own inputs are `tests/corpus_run/*.ts`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use spot_ts::{
    Compiled, EngineConfig, HostBridge, HostError, HostSurface, Limits, ScriptEngine, ScriptError,
    ScriptId,
};
use spot_ts_transpile::{Options, Transpiled, transpile};

/// Both ways a script can run: from cached bytecode and from source.
const MODES: [bool; 2] = [true, false];

/// Corpus cases the engine cannot load, with the QuickJS limitation that explains it. Empty: a
/// case that does not load is a finding, not an entry here.
const KNOWN_NOT_LOADABLE: &[(&str, &str)] = &[];

const ACCEPTED: usize = 60;
const REJECTED: usize = 22;

struct NoHost;

impl HostBridge for NoHost {
    fn call(&self, name: &str, _args: Value) -> Result<Value, HostError> {
        Err(HostError::new("unknown", name))
    }
    fn now_millis(&self) -> i64 {
        0
    }
    fn random_seed(&self) -> u64 {
        1
    }
}

fn engine(keep: bool) -> ScriptEngine {
    ScriptEngine::new(EngineConfig {
        surface: HostSurface::new(Vec::new()),
        keep_bytecode: keep,
        ..EngineConfig::default()
    })
}

fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn corpus_root() -> PathBuf {
    here().join("../spot-ts-transpile/tests/corpus")
}

fn run_dir() -> PathBuf {
    here().join("tests/corpus_run")
}

/// The source of a corpus case or of an own input.
fn source_of(case: &str) -> String {
    let corpus = corpus_root().join(case).join("input.ts");
    let path = if corpus.is_file() {
        corpus
    } else {
        run_dir().join(format!("{case}.ts"))
    };
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn emit(case: &str) -> Transpiled {
    let file = format!("{case}.ts");
    transpile(&source_of(case), &file, &Options::default())
        .unwrap_or_else(|d| panic!("case {case} must transpile: {d:?}"))
}

fn compile(
    engine: &ScriptEngine,
    case: &str,
    t: &Transpiled,
) -> Result<Arc<Compiled>, ScriptError> {
    engine.compile(&t.js, ScriptId::new("corpus", case, 1), Some(&t.source_map))
}

fn run(
    engine: &ScriptEngine,
    compiled: &Compiled,
    entry: &str,
    args: &Value,
) -> Result<Value, ScriptError> {
    engine.run_blocking(compiled, entry, args, Arc::new(NoHost), Limits::default())
}

fn case_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(corpus_root())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().join("input.ts").is_file())
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

// ---- part 1: every case ----

#[test]
fn every_corpus_case_is_loaded_by_the_engine_or_refused_by_the_transpiler() {
    for keep in MODES {
        let e = engine(keep);
        let (mut accepted, mut rejected, mut with_imports) = (0, 0, 0);
        for case in case_names() {
            let wants_js = corpus_root().join(&case).join("expected.js").is_file();
            let result = transpile(&source_of(&case), "corpus.ts", &Options::default());
            match (wants_js, result) {
                (true, Ok(t)) => {
                    accepted += 1;
                    if KNOWN_NOT_LOADABLE.iter().any(|(c, _)| *c == case) {
                        continue;
                    }
                    let compiled = e.compile(
                        &t.js,
                        ScriptId::new("corpus", &case, 1),
                        Some(&t.source_map),
                    );
                    if t.imports.is_empty() {
                        assert!(
                            compiled.is_ok(),
                            "case {case} (keep_bytecode={keep}): the engine cannot compile what \
                             the transpiler emitted: {:?}\n{}",
                            compiled.err(),
                            t.js
                        );
                    } else {
                        with_imports += 1;
                        let err = compiled.err().unwrap_or_else(|| {
                            panic!("case {case}: an import must be refused by the engine")
                        });
                        assert!(
                            matches!(&err, ScriptError::Compile(m) if m.contains("imports are not allowed")),
                            "case {case}: {err:?}"
                        );
                    }
                }
                (false, Err(_)) => rejected += 1,
                (true, Err(d)) => panic!("case {case} must be accepted: {d:?}"),
                (false, Ok(_)) => panic!("case {case} must be rejected"),
            }
        }
        assert_eq!(accepted, ACCEPTED, "the accepted corpus shrank or grew");
        assert_eq!(rejected, REJECTED, "the rejected corpus shrank or grew");
        assert!(with_imports > 0, "the corpus has programs with imports");
    }
}

// ---- part 2: the run specs ----

#[test]
fn the_run_specs_behave_like_the_typescript_they_came_from() {
    let mut specs: Vec<PathBuf> = std::fs::read_dir(run_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    specs.sort();
    assert!(
        specs.len() >= 30,
        "at least 30 run specs, found {}",
        specs.len()
    );
    let mut ran = 0;
    for keep in MODES {
        let e = engine(keep);
        for path in &specs {
            let spec: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap())
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            let case = spec["case"].as_str().unwrap();
            let entry = spec["entry"].as_str().unwrap();
            let t = emit(case);
            let compiled = compile(&e, case, &t)
                .unwrap_or_else(|err| panic!("{}: compile: {err}", path.display()));
            let got = run(&e, &compiled, entry, &spec["args"])
                .unwrap_or_else(|err| panic!("{} (keep_bytecode={keep}): {err}", path.display()));
            assert_eq!(
                got,
                spec["expect"],
                "{} (keep_bytecode={keep}): the emitted JavaScript behaves differently\n{}",
                path.display(),
                t.js
            );
            ran += 1;
        }
    }
    assert!(ran >= 60, "30 specs in two modes, ran {ran}");
}

#[test]
fn the_specs_cover_the_required_features() {
    let names: BTreeSet<String> = std::fs::read_dir(run_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    for needle in [
        "class_fields_run",
        "parameter_properties_run",
        "abstract_run",
        "enums_run",
        "enum_reverse",
        "const_enum_inlined",
        "generics_and_assertions_run",
        "non_null_assertion",
        "optional_chain",
        "nullish_coalesce",
        "map_for_of_run",
        "generator_run",
        "async_run",
        "destructuring_run",
        "export_default_function",
        "accessors_run",
        "static_members_run",
        "import_type_only",
    ] {
        assert!(
            names.iter().any(|n| n.contains(needle)),
            "no run spec covers {needle}"
        );
    }
}

#[test]
fn decorators_and_namespaces_are_rejected_by_the_transpiler() {
    for case in [
        "r_decorated_class",
        "r_decorated_method",
        "r_namespace_block",
    ] {
        assert!(
            transpile(&source_of(case), "corpus.ts", &Options::default()).is_err(),
            "{case} must be rejected"
        );
    }
}

// ---- part 3: source maps through the engine ----

fn thrown_at(case: &str, entry: &str) -> (String, String, Option<spot_ts::SourcePosition>) {
    let t = emit(case);
    let mut seen = Vec::new();
    for keep in MODES {
        let e = engine(keep);
        let compiled = compile(&e, case, &t).unwrap();
        match run(&e, &compiled, entry, &json!(null)) {
            Err(ScriptError::Thrown {
                name,
                message,
                position,
                ..
            }) => seen.push((name, message, position)),
            other => panic!("{case}/{entry}: expected an exception, got {other:?}"),
        }
    }
    assert_eq!(seen[0], seen[1], "both modes report the same position");
    seen.remove(0)
}

fn assert_line(position: Option<spot_ts::SourcePosition>, file: &str, line: u32, what: &str) {
    let p = position.unwrap_or_else(|| panic!("{what}: the error carries no source position"));
    assert_eq!(p.source, file, "{what}");
    assert_eq!(p.line, line, "{what}: the TypeScript line of the throw");
    assert!(p.column >= 1, "{what}");
}

#[test]
fn a_throw_in_a_method_is_reported_at_its_typescript_line() {
    let (name, message, position) = thrown_at("own_throws", "in_method");
    assert_eq!((name.as_str(), message.as_str()), ("Error", "method"));
    assert_line(position, "own_throws.ts", 3, "method");
}

#[test]
fn a_throw_in_an_arrow_field_initialiser_is_reported_at_its_typescript_line() {
    let (name, message, position) = thrown_at("own_throws", "in_field_arrow");
    assert_eq!(
        (name.as_str(), message.as_str()),
        ("TypeError", "field arrow")
    );
    assert_line(position, "own_throws.ts", 13, "field arrow");
}

#[test]
fn a_throw_after_an_await_is_reported_at_its_typescript_line() {
    let (name, message, position) = thrown_at("own_throws", "in_async");
    assert_eq!(
        (name.as_str(), message.as_str()),
        ("RangeError", "after await")
    );
    assert_line(position, "own_throws.ts", 23, "after await");
}

#[test]
fn a_throw_in_an_enum_member_computation_is_reported_at_its_typescript_line() {
    let (_, message, position) = thrown_at("own_throws_enum", "in_enum");
    assert_eq!(message, "enum member");
    assert_line(position, "own_throws_enum.ts", 4, "enum member");
}

#[test]
fn a_typed_error_is_a_validation_error_with_its_code_and_fields() {
    let t = emit("own_throws");
    for keep in MODES {
        let e = engine(keep);
        let compiled = compile(&e, "own_throws", &t).unwrap();
        let err = run(&e, &compiled, "typed_error", &json!(null)).unwrap_err();
        let ScriptError::Validation {
            code,
            message,
            fields,
        } = err
        else {
            panic!("expected a validation error, got {err:?}")
        };
        assert_eq!(code, "app.too_small");
        assert_eq!(message, "the quantity is too small");
        assert_eq!(fields.len(), 1);
        assert_eq!(
            (
                fields[0].field.as_str(),
                fields[0].code.as_str(),
                fields[0].message.as_str()
            ),
            ("qty", "app.qty_min", "at least 1")
        );
    }
}

// ---- part 4: the conformance statements ----

fn run_case(case: &str, entry: &str, args: Value) -> Value {
    let t = emit(case);
    let mut out = Vec::new();
    for keep in MODES {
        let e = engine(keep);
        let compiled = compile(&e, case, &t).unwrap();
        out.push(run(&e, &compiled, entry, &args).unwrap());
    }
    assert_eq!(out[0], out[1]);
    out.remove(0)
}

#[test]
fn quickjs_runs_class_fields_the_transpiler_emits() {
    let t = emit("class_fields");
    assert!(
        t.js.contains("#used = 0;") && t.js.contains("static MAX = 100;"),
        "{}",
        t.js
    );
    assert_eq!(
        run_case("own_runtime", "class_fields_run", json!(3)),
        json!(21)
    );
}

#[test]
fn parameter_properties_become_assigned_fields() {
    let got = run_case("own_runtime", "parameter_properties_run", json!(3));
    assert_eq!(got["ownKeys"], json!(["x", "y", "tag"]));
    assert_eq!(got["sum"], json!(7));
}

#[test]
fn abstract_methods_are_absent_from_the_emitted_class() {
    let t = emit("abstract_class");
    assert!(!t.js.contains("abstract"), "{}", t.js);
    assert_eq!(
        run_case("own_runtime", "abstract_run", json!(null)),
        json!("shape:square")
    );
    assert_eq!(
        run_case("own_runtime", "abstract_base_call", json!(null)),
        json!("TypeError")
    );
}

#[test]
fn enum_iife_with_pure_annotation_runs() {
    let t = emit("enum_color");
    assert!(t.js.contains("/* @__PURE__ */"), "{}", t.js);
    let e = engine(true);
    let compiled = compile(&e, "enum_color", &t).unwrap();
    assert_eq!(
        run(&e, &compiled, "label", &json!(2)).unwrap(),
        json!("Blue")
    );
    assert_eq!(
        run_case("own_runtime", "enums_run", json!(null))["reverse"],
        json!("B")
    );
}

#[test]
fn eval_and_new_function_are_refused_by_the_transpiler_and_the_engine_has_no_eval() {
    for case in [
        "r_eval_call",
        "r_new_function",
        "r_function_call_form",
        "r_bare_eval_reference",
    ] {
        assert!(
            transpile(&source_of(case), "corpus.ts", &Options::default()).is_err(),
            "{case}: the transpiler must refuse it at save"
        );
    }
    for keep in MODES {
        let e = engine(keep);
        let compiled = e
            .compile(
                "export function f() { return eval('1 + 1'); }",
                ScriptId::new("corpus", "hand_eval", 1),
                None,
            )
            .unwrap();
        let err = run(&e, &compiled, "f", &json!(null)).unwrap_err();
        let ScriptError::Thrown { name, message, .. } = &err else {
            panic!("{err:?}")
        };
        assert_eq!(err.code(), "script.thrown");
        assert_eq!(name, "TypeError");
        assert!(message.contains("disabled"), "{message}");
    }
}

#[test]
fn a_module_with_an_import_of_anything_but_itself_is_refused_by_the_engine() {
    for keep in MODES {
        let e = engine(keep);
        for (n, src) in [
            "import x from './other.js'; export function f() { return x; }",
            "import { audit } from 'spot:audit'; export function f() { return audit; }",
            "export { y } from './other.js';",
        ]
        .iter()
        .enumerate()
        {
            let err = e
                .compile(src, ScriptId::new("corpus", &format!("imp{n}"), 1), None)
                .unwrap_err();
            assert_eq!(err.code(), "script.compile", "{src}");
            assert!(
                err.to_string().contains("imports are not allowed"),
                "{src}: {err}"
            );
        }
    }
}
