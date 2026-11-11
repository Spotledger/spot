//! Running scripts: results, errors, the allow-list, determinism, isolation and the cache.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use spot_app_api::ErrorClass;
use spot_ts::*;

fn engine(keep_bytecode: bool) -> ScriptEngine {
    ScriptEngine::new(EngineConfig {
        surface: surface(),
        keep_bytecode,
        ..EngineConfig::default()
    })
}

/// Both ways a script can run: from cached bytecode and from source. Everything below must hold
/// for each.
const MODES: [bool; 2] = [true, false];

fn compile(engine: &ScriptEngine, source: &str) -> Arc<Compiled> {
    engine
        .compile(source, ScriptId::new("app", "script", 1), None)
        .unwrap()
}

fn run(engine: &ScriptEngine, source: &str, entry: &str, args: Value) -> Result<Value> {
    let compiled = compile(engine, source);
    engine.run_blocking(
        &compiled,
        entry,
        &args,
        Arc::new(Recorder::new()),
        Limits::default(),
    )
}

#[test]
fn a_script_gets_its_argument_and_returns_json() {
    for keep in MODES {
        let e = engine(keep);
        let src = "export function on_event(doc) { return { total: doc.items.map(i => i.qty * i.rate).reduce((a, b) => a + b, 0), n: doc.items.length }; }";
        let doc = json!({"items": [{"qty": 2, "rate": 5}, {"qty": 3, "rate": 7}]});
        assert_eq!(
            run(&e, src, "on_event", doc).unwrap(),
            json!({"total": 31, "n": 2}),
            "keep_bytecode={keep}"
        );
        // undefined becomes null; a promise is awaited.
        assert_eq!(
            run(&e, "export function f() {}", "f", json!(null)).unwrap(),
            Value::Null
        );
        assert_eq!(
            run(
                &e,
                "export async function f(x) { return await Promise.resolve(x + 1); }",
                "f",
                json!(41)
            )
            .unwrap(),
            json!(42)
        );
    }
}

#[test]
fn bytecode_and_source_runs_agree() {
    let src = "export function f(x) { let s = 0; for (let i = 0; i < x; i++) { s += i * i; } return { s, text: `n=${x}` }; }";
    let with = run(&engine(true), src, "f", json!(100)).unwrap();
    let without = run(&engine(false), src, "f", json!(100)).unwrap();
    assert_eq!(with, without);
    assert!(compile(&engine(true), src).has_bytecode());
    assert!(!compile(&engine(false), src).has_bytecode());
}

#[test]
fn compile_and_entry_errors_are_typed() {
    for keep in MODES {
        let e = engine(keep);
        let err = e
            .compile("export function (", ScriptId::new("app", "bad", 1), None)
            .unwrap_err();
        assert_eq!(err.code(), "script.compile", "{err}");
        let err = run(&e, "export function f() { return 1; }", "nope", json!(null)).unwrap_err();
        assert_eq!(err, ScriptError::MissingEntry("nope".into()));
        let err = run(&e, "export const f = 5;", "f", json!(null)).unwrap_err();
        assert_eq!(
            err.code(),
            "script.missing_entry",
            "an export that is not a function has no entry"
        );
        // `args` that cannot be serialised do not exist in JSON, but a bad entry name does not panic.
        assert_eq!(
            run(&e, "export function f() { return 1; }", "", json!(null))
                .unwrap_err()
                .code(),
            "script.missing_entry"
        );
    }
}

#[test]
fn the_global_object_has_the_language_and_nothing_else() {
    for keep in MODES {
        let e = engine(keep);
        let src =
            "export function names() { return Object.getOwnPropertyNames(globalThis).sort(); }";
        let names: Vec<String> =
            serde_json::from_value(run(&e, src, "names", json!(null)).unwrap()).unwrap();
        // The language's own built-ins (ECMAScript), `queueMicrotask`, and `host`: that is the list.
        for forbidden in [
            "require",
            "module",
            "exports",
            "process",
            "fetch",
            "XMLHttpRequest",
            "WebSocket",
            "setTimeout",
            "setInterval",
            "setImmediate",
            "console",
            "print",
            "std",
            "os",
            "Deno",
            "Bun",
            "WebAssembly",
            "performance",
            "navigator",
            "window",
            "document",
            "global",
            "Worker",
        ] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "`{forbidden}` is reachable (keep_bytecode={keep}): {names:?}"
            );
        }
        assert!(names.iter().any(|n| n == "host"));
        for builtin in [
            "Object", "Array", "JSON", "Math", "Date", "Map", "Set", "Promise", "RegExp",
        ] {
            assert!(names.iter().any(|n| n == builtin), "{builtin} missing");
        }
    }
}

#[test]
fn code_cannot_be_made_from_strings() {
    for keep in MODES {
        let e = engine(keep);
        for (label, body) in [
            ("eval", "eval('1 + 1')"),
            ("indirect eval", "(0, globalThis.eval)('1')"),
            ("Function", "new Function('return 1')()"),
            ("Function call", "Function('return 1')()"),
            (
                "constructor of a function",
                "(function () {}).constructor('return 1')()",
            ),
            (
                "constructor of an arrow",
                "(() => 1).constructor('return 1')()",
            ),
            (
                "AsyncFunction",
                "(async function () {}).constructor('return 1')",
            ),
            (
                "GeneratorFunction",
                "(function* () {}).constructor('yield 1')",
            ),
            (
                "AsyncGeneratorFunction",
                "(async function* () {}).constructor('yield 1')",
            ),
            (
                "prototype constructor",
                "Object.getPrototypeOf(function () {}).constructor('return 1')",
            ),
        ] {
            let src = format!("export function f() {{ return {body}; }}");
            let err = run(&e, &src, "f", json!(null)).unwrap_err();
            let ScriptError::Thrown { name, message, .. } = &err else {
                panic!("{label}: {err:?}")
            };
            assert_eq!(name, "TypeError", "{label}: {err}");
            assert!(message.contains("disabled"), "{label}: {err}");
        }
    }
}

#[test]
fn imports_of_anything_fail() {
    for keep in MODES {
        let e = engine(keep);
        // A static import does not compile: the reason is in the error.
        for src in [
            "import x from './other.js'; export function f() { return 1; }",
            "import fs from 'fs'; export function f() { return fs; }",
            "import u from 'https://example.com/x.js'; export function f() { return 1; }",
            "export { y } from './other.js';",
        ] {
            let err = e
                .compile(src, ScriptId::new("app", "imp", 1), None)
                .unwrap_err();
            assert!(
                matches!(&err, ScriptError::Compile(m) if m.contains("imports are not allowed")),
                "{src}: {err:?}"
            );
        }
        // A dynamic import, and `require`, fail when they run, as an ordinary throw.
        for src in [
            "export async function f() { return await import('./other.js'); }",
            "export function f() { return require('fs'); }",
        ] {
            let err = run(&e, src, "f", json!(null)).unwrap_err();
            assert!(matches!(err, ScriptError::Thrown { .. }), "{src}: {err:?}");
        }
    }
}

#[test]
fn time_and_randomness_come_from_the_invocation() {
    for keep in MODES {
        let e = engine(keep);
        let src = "export function f() { return { now: host.now(), date: Date.now(), made: new Date().getTime(), fixed: new Date(86400000).toISOString(), parsed: Date.UTC(2026, 0, 1), r: [Math.random(), Math.random(), host.random()] }; }";
        let compiled = compile(&e, src);
        let go = |seed: u64| {
            let bridge = Recorder {
                seed,
                ..Recorder::new()
            };
            e.run_blocking(
                &compiled,
                "f",
                &json!(null),
                Arc::new(bridge),
                Limits::default(),
            )
            .unwrap()
        };
        let a = go(7);
        assert_eq!(a["now"], json!(1_790_000_000_000_i64));
        assert_eq!(a["date"], a["now"]);
        assert_eq!(a["made"], a["now"]);
        assert_eq!(
            a["fixed"],
            json!("1970-01-02T00:00:00.000Z"),
            "explicit dates still work"
        );
        assert_eq!(a["parsed"], json!(1_767_225_600_000_i64));
        let r = a["r"].as_array().unwrap();
        assert!(
            r.iter().all(
                |v| v.as_str().is_none() && v.as_f64().is_some_and(|x| (0.0..1.0).contains(&x))
            ),
            "{r:?}"
        );
        assert_ne!(r[0], r[1]);
        // The same seed, the same numbers; another seed, other numbers.
        assert_eq!(go(7), a);
        assert_ne!(go(8)["r"], a["r"]);
    }
}

#[test]
fn host_is_frozen_and_lists_exactly_the_allow_list() {
    for keep in MODES {
        let e = engine(keep);
        let src = r#"
            export function f() {
              const out = { keys: Object.keys(host).sort(), frozen: Object.isFrozen(host) };
              for (const [label, attempt] of [
                ["assign", () => { host.get_doc = () => 1; }],
                ["add", () => { host.extra = 1; }],
                ["delete", () => { delete host.echo; }],
                ["replace host", () => { globalThis.host = {}; }],
                ["redefine Date", () => { globalThis.Date = 1; }],
                ["redefine eval", () => { globalThis.eval = () => 1; }],
                ["redefine Math.random", () => { Math.random = () => 1; }],
              ]) {
                try { attempt(); out[label] = "allowed"; } catch (e) { out[label] = e.name; }
              }
              out.missing = typeof host.send_mail + typeof host.fs + typeof host.__bits + typeof host.call;
              return out;
            }
        "#;
        let out = run(&e, src, "f", json!(null)).unwrap();
        assert_eq!(
            out["keys"],
            json!(["boom", "echo", "get_doc", "now", "ping", "random"])
        );
        assert_eq!(out["frozen"], json!(true));
        for label in [
            "assign",
            "add",
            "delete",
            "replace host",
            "redefine Date",
            "redefine eval",
            "redefine Math.random",
        ] {
            assert_eq!(out[label], json!("TypeError"), "{label}");
        }
        assert_eq!(
            out["missing"],
            json!("undefinedundefinedundefinedundefined")
        );
    }
}

#[test]
fn host_calls_are_checked_against_their_schema_and_errors_are_catchable() {
    for keep in MODES {
        let e = engine(keep);
        let src = r#"
            export function f() {
              const out = {};
              out.doc = host.get_doc({ doctype: "customer", id: "c-1" });
              try { host.get_doc({ doctype: "customer", id: "missing" }); } catch (e) { out.missing = [e.name, e.code, e.message]; }
              for (const [label, arg] of [["no id", { doctype: "customer" }], ["extra", { doctype: "c", id: "x", more: 1 }], ["wrong type", { doctype: 1, id: "x" }], ["not an object", "c-1"], ["none", undefined]]) {
                try { host.get_doc(arg); out[label] = "allowed"; } catch (e) { out[label] = [e.name, e.code]; }
              }
              out.echo = host.echo({ a: [1, 2, { b: null }], c: "é" });
              return out;
            }
        "#;
        let bridge = Arc::new(Recorder::new());
        let compiled = compile(&e, src);
        let out = e
            .run_blocking(
                &compiled,
                "f",
                &json!(null),
                bridge.clone(),
                Limits::default(),
            )
            .unwrap();
        assert_eq!(out["doc"], json!({"name": "Acme", "credit": 100}));
        assert_eq!(
            out["missing"],
            json!(["HostError", "not_found", "no such document"])
        );
        for label in ["no id", "extra", "wrong type", "not an object", "none"] {
            assert_eq!(out[label], json!(["HostError", "bad_arguments"]), "{label}");
        }
        assert_eq!(out["echo"], json!({"a": [1, 2, {"b": null}], "c": "é"}));
        // Only the calls that passed the schema reached the bridge.
        let names: Vec<String> = bridge.calls().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["get_doc", "get_doc", "echo"]);
    }
}

#[test]
fn the_same_inputs_give_the_same_result_and_call_sequence() {
    for keep in MODES {
        let e = engine(keep);
        let src = r#"
            export function f(ids) {
              const seen = [];
              for (const id of ids) {
                try { seen.push(host.get_doc({ doctype: "customer", id }).name); } catch (e) { seen.push(e.code); }
                seen.push(Math.random());
              }
              return { seen, at: host.now() };
            }
        "#;
        let compiled = compile(&e, src);
        let go = || {
            let bridge = Arc::new(Recorder::new());
            let out = e
                .run_blocking(
                    &compiled,
                    "f",
                    &json!(["c-1", "zz", "c-1"]),
                    bridge.clone(),
                    Limits::default(),
                )
                .unwrap();
            (out, bridge.calls())
        };
        assert_eq!(go(), go());
        assert_eq!(go().1.len(), 3);
    }
}

#[test]
fn nothing_is_shared_between_executions() {
    for keep in MODES {
        let e = engine(keep);
        let src = "export function f() { const before = typeof globalThis.leak + typeof host.extra; globalThis.leak = 1; Object.prototype.polluted = 1; Array.prototype.x = 2; return before; }";
        let compiled = compile(&e, src);
        for _ in 0..3 {
            let out = e
                .run_blocking(
                    &compiled,
                    "f",
                    &json!(null),
                    Arc::new(Recorder::new()),
                    Limits::default(),
                )
                .unwrap();
            assert_eq!(out, json!("undefinedundefined"));
        }
        // A pollution of the built-ins in one run is not there in a run of another script.
        let probe = "export function f() { return [typeof ({}).polluted, typeof [].x]; }";
        assert_eq!(
            run(&e, probe, "f", json!(null)).unwrap(),
            json!(["undefined", "undefined"])
        );
    }
}

#[test]
fn script_errors_say_what_was_thrown() {
    for keep in MODES {
        let e = engine(keep);
        let err = run(
            &e,
            "export function f() { throw new RangeError('too big'); }",
            "f",
            json!(null),
        )
        .unwrap_err();
        let ScriptError::Thrown {
            name,
            message,
            stack,
            position,
        } = &err
        else {
            panic!("{err:?}")
        };
        assert_eq!((name.as_str(), message.as_str()), ("RangeError", "too big"));
        assert!(
            stack.as_deref().is_some_and(|s| s.contains("script:")),
            "{stack:?}"
        );
        assert!(position.is_none(), "no source map was given");
        assert_eq!(err.code(), "script.thrown");
        // Non-error values are thrown too.
        let err = run(
            &e,
            "export function f() { throw 'plain text'; }",
            "f",
            json!(null),
        )
        .unwrap_err();
        assert!(
            matches!(&err, ScriptError::Thrown { name, message, .. } if name == "Thrown" && message.contains("plain text")),
            "{err:?}"
        );
        // An uncaught host error keeps its name.
        let err = run(
            &e,
            "export function f() { host.get_doc({ doctype: 'customer', id: 'nobody' }); }",
            "f",
            json!(null),
        )
        .unwrap_err();
        assert!(
            matches!(&err, ScriptError::Thrown { name, message, .. } if name == "HostError" && message == "no such document"),
            "{err:?}"
        );
        let err = run(
            &e,
            "export function f() { return null.x; }",
            "f",
            json!(null),
        )
        .unwrap_err();
        assert!(
            matches!(&err, ScriptError::Thrown { name, .. } if name == "TypeError"),
            "{err:?}"
        );
    }
}

#[test]
fn a_source_map_names_the_original_position() {
    // Generated line 3 maps to line 10, column 5 of `orig.ts` ("AASI": +0 column, +0 source, +9 lines, +4 columns).
    let map = r#"{"version":3,"sources":["orig.ts"],"mappings":";;AASI"}"#;
    let src = "export function f() {\n  const x = 1;\n  throw new Error('mapped');\n}";
    for keep in MODES {
        let e = engine(keep);
        let compiled = e
            .compile(
                src,
                ScriptId::new("app", "mapped", u64::from(keep)),
                Some(map),
            )
            .unwrap();
        let err = e
            .run_blocking(
                &compiled,
                "f",
                &json!(null),
                Arc::new(Recorder::new()),
                Limits::default(),
            )
            .unwrap_err();
        let ScriptError::Thrown { position, .. } = err else {
            panic!("{err:?}")
        };
        let position = position.expect("the throw is on a mapped line");
        assert_eq!(
            position,
            SourcePosition {
                source: "orig.ts".into(),
                line: 10,
                column: 5
            }
        );
    }
    // A malformed map is refused at compile time.
    let err = engine(true)
        .compile(src, ScriptId::new("app", "bad", 1), Some("not a map"))
        .unwrap_err();
    assert_eq!(err.code(), "script.compile");
}

#[test]
fn a_typed_throw_is_a_validation_error_and_maps_to_an_app_error() {
    for keep in MODES {
        let e = engine(keep);
        let src = r#"
            export function plain() { throw { code: "selling.qty_not_positive", message: "Quantity must be positive." }; }
            export function fields() {
              throw { code: "selling.bad_lines", message: "Two lines are wrong.",
                      fields: [{ field: "items[2].qty", code: "selling.qty_not_positive", message: "Positive, please." },
                               { field: "customer", code: "selling.customer_blocked", message: "Blocked." },
                               { nonsense: true }] };
            }
            export function as_error() { const e = new Error("x"); e.code = "app.thing"; throw e; }
        "#;
        let err = run(&e, src, "plain", json!(null)).unwrap_err();
        assert_eq!(
            err,
            ScriptError::Validation {
                code: "selling.qty_not_positive".into(),
                message: "Quantity must be positive.".into(),
                fields: vec![]
            }
        );
        let app = err.into_app_error();
        assert_eq!(app.class, ErrorClass::Validation);
        assert_eq!(app.code, "selling.qty_not_positive");
        assert_eq!(app.message, "Quantity must be positive.");

        let app = run(&e, src, "fields", json!(null))
            .unwrap_err()
            .into_app_error();
        assert_eq!(app.class, ErrorClass::Validation);
        assert_eq!(app.code, "document.validation_failed");
        assert_eq!(app.message, "Two lines are wrong.");
        let fields: Vec<(String, String)> = app
            .fields
            .iter()
            .map(|f| (f.field.as_str().to_owned(), f.code.clone()))
            .collect();
        assert_eq!(
            fields,
            [
                ("items[2].qty".into(), "selling.qty_not_positive".into()),
                ("customer".into(), "selling.customer_blocked".into())
            ]
        );

        // An `Error` carrying a code is typed too, and a host error's `code` is not a validation.
        assert!(
            matches!(run(&e, src, "as_error", json!(null)).unwrap_err(), ScriptError::Validation { code, .. } if code == "app.thing")
        );

        // Everything else is an engine or script fault: class `Internal`, the `script.*` code.
        let app = run(
            &e,
            "export function f() { throw new Error('bug'); }",
            "f",
            json!(null),
        )
        .unwrap_err()
        .into_app_error();
        assert_eq!(
            (app.class, app.code.as_str()),
            (ErrorClass::Internal, "script.thrown")
        );
        let app = ScriptError::Timeout { millis: 5 }.into_app_error();
        assert_eq!(
            (app.class, app.code.as_str()),
            (ErrorClass::Internal, "script.timeout")
        );
        assert!(
            !app.message.contains("function"),
            "no script source in messages"
        );
    }
}

#[test]
fn a_panicking_host_function_stops_the_run_and_nothing_else() {
    for keep in MODES {
        let e = engine(keep);
        let src = "export function f() { try { host.boom(1); } catch (err) { /* the script may catch it */ } for (;;) {} }";
        let started = Instant::now();
        let err = run(&e, src, "f", json!(null)).unwrap_err();
        assert_eq!(err, ScriptError::HostPanic("boom".into()));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the run was stopped, not left to time out"
        );
        // The engine is fine: it compiles and runs again, and its stats are readable.
        assert_eq!(
            run(&e, "export function f() { return 1; }", "f", json!(null)).unwrap(),
            json!(1)
        );
        assert!(e.stats().compiles >= 2);
    }
}

#[test]
fn the_cache_compiles_once_per_script_version() {
    let e = engine(true);
    let id = |v| ScriptId::new("sales", "on_save", v);
    let src = "export function f() { return 1; }";
    let a = e.compile(src, id(1), None).unwrap();
    let b = e.compile(src, id(1), None).unwrap();
    assert!(Arc::ptr_eq(&a, &b), "an unchanged script is not recompiled");
    assert_eq!((e.stats().compiles, e.stats().cache_hits), (1, 1));
    // A new version is a new entry (even with the same text), and both stay.
    let c = e.compile(src, id(2), None).unwrap();
    assert!(!Arc::ptr_eq(&a, &c));
    assert_eq!((e.stats().compiles, e.stats().cached_scripts), (2, 2));
    // The same id and version with other text is a change: recompiled and replaced.
    let d = e
        .compile("export function f() { return 2; }", id(2), None)
        .unwrap();
    assert_ne!(c.source_hash(), d.source_hash());
    assert_eq!((e.stats().compiles, e.stats().cached_scripts), (3, 2));
    assert_eq!(
        e.run_blocking(
            &d,
            "f",
            &json!(null),
            Arc::new(Recorder::new()),
            Limits::default()
        )
        .unwrap(),
        json!(2)
    );
    // Other apps and names are separate.
    e.compile(src, ScriptId::new("purchasing", "on_save", 1), None)
        .unwrap();
    assert_eq!(e.stats().cached_scripts, 3);
    assert_eq!(a.source_hash().len(), 64);
    assert!(a.bytecode_len() > 0 && a.has_bytecode());
    assert_eq!(a.id(), &id(1));
}

#[test]
fn the_cache_evicts_the_least_recently_used_script_within_its_cap() {
    let big = |n: usize| format!("export function f() {{ return {}1; }}", "1 + ".repeat(n));
    // What one script of this size weighs in the cache.
    let one = {
        let probe = ScriptEngine::new(EngineConfig::default());
        probe
            .compile(&big(300), ScriptId::new("a", "x", 0), None)
            .unwrap();
        probe.stats().cached_bytes
    };
    assert!(one > 0);
    let cap = one * 3 + one / 2;
    let e = ScriptEngine::new(EngineConfig {
        cache_bytes: cap,
        ..EngineConfig::default()
    });
    let ids: Vec<ScriptId> = (1..=5).map(|v| ScriptId::new("a", "x", v)).collect();
    for id in &ids[..3] {
        e.compile(&big(300), id.clone(), None).unwrap();
    }
    assert_eq!(e.stats().cached_scripts, 3);
    // Use #1 so that #2 is the least recently used, then add #4: #2 goes.
    e.compile(&big(300), ids[0].clone(), None).unwrap();
    e.compile(&big(300), ids[3].clone(), None).unwrap();
    assert!(e.is_cached(&ids[0]) && e.is_cached(&ids[2]) && e.is_cached(&ids[3]));
    assert!(
        !e.is_cached(&ids[1]),
        "the least recently used entry was evicted"
    );
    assert!(e.stats().cached_bytes <= cap);
    // The newest entry always stays, even if it alone is over the cap.
    let tiny = ScriptEngine::new(EngineConfig {
        cache_bytes: 1,
        ..EngineConfig::default()
    });
    tiny.compile(&big(10), ids[0].clone(), None).unwrap();
    assert_eq!(tiny.stats().cached_scripts, 1);
    // An evicted script still runs: the caller holds its `Compiled`.
    let first = tiny
        .compile("export function f() { return 7; }", ids[1].clone(), None)
        .unwrap();
    tiny.compile("export function f() { return 8; }", ids[2].clone(), None)
        .unwrap();
    assert!(!tiny.is_cached(&ids[1]));
    let out = tiny
        .run_blocking(
            &first,
            "f",
            &json!(null),
            Arc::new(Recorder::new()),
            Limits::default(),
        )
        .unwrap();
    assert_eq!(out, json!(7));
}

#[test]
fn identical_bytecode_is_interned_once() {
    let src = format!(
        "export function unique_{}() {{ return 1; }}",
        std::process::id()
    );
    let a = engine(true)
        .compile(&src, ScriptId::new("app", "i", 1), None)
        .unwrap();
    // Another engine, another app, name and version, the same source: the very same bytes.
    let b = engine(true)
        .compile(&src, ScriptId::new("other", "j", 9), None)
        .unwrap();
    assert!(a.shares_bytecode_with(&b));
    let c = engine(true)
        .compile(
            "export function other() { return 2; }",
            ScriptId::new("app", "k", 1),
            None,
        )
        .unwrap();
    assert!(!a.shares_bytecode_with(&c));
    // A script that runs from source shares nothing.
    let d = engine(false)
        .compile(&src, ScriptId::new("app", "i", 1), None)
        .unwrap();
    assert!(!a.shares_bytecode_with(&d));
    // The process-wide budget bounds what stays interned.
    assert!(interned_bytes() >= a.bytecode_len());
    assert!(interned_bytes() <= INTERN_BUDGET);
}
