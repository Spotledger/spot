//! The contract-validation matrix: malformed modules fail the load with every problem reported.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_ts_controllers::{TsLimits, load_in_memory};

/// A module whose `contract()` returns `expr`.
fn module(expr: &str) -> String {
    format!(
        "export function contract() {{ return {expr}; }}\n\
         export function run_step() {{ return null; }}\n\
         export function run_method() {{ return null; }}\n"
    )
}

fn problems(expr: &str) -> Vec<spot_ts_controllers::Problem> {
    load_in_memory(
        "app",
        "controllers/m.ts",
        &module(expr),
        TsLimits::default(),
    )
    .err()
    .expect("the module must not load")
}

#[test]
fn a_good_module_loads() {
    let good = module(
        r#"{ doctype: "note", steps: { fill: { point: "before_save" } },
             methods: { ping: { right: "read", args: { amount: "decimal?" }, result: { ok: "bool" } } } }"#,
    );
    load_in_memory("app", "controllers/m.ts", &good, TsLimits::default())
        .expect("the good module loads");
}

#[test]
fn the_twelve_malformed_modules_are_refused() {
    let cases: [(&str, &str); 12] = [
        ("not an object", "42"),
        ("no doctype", r#"{ "steps": {}, "methods": {} }"#),
        (
            "doctype is not a storage name",
            r#"{ "doctype": "Not A Name" }"#,
        ),
        (
            "steps is not an object",
            r#"{ "doctype": "note", "steps": [] }"#,
        ),
        (
            "a step has no point",
            r#"{ "doctype": "note", "steps": { "s": {} } }"#,
        ),
        (
            "a step point is unknown",
            r#"{ "doctype": "note", "steps": { "s": { "point": "whenever" } } }"#,
        ),
        (
            "a step priority is not an integer",
            r#"{ "doctype": "note", "steps": { "s": { "point": "before_save", "priority": "high" } } }"#,
        ),
        (
            "methods is not an object",
            r#"{ "doctype": "note", "methods": 7 }"#,
        ),
        (
            "a method has no right",
            r#"{ "doctype": "note", "methods": { "m": {} } }"#,
        ),
        (
            "a method right is not a name",
            r#"{ "doctype": "note", "methods": { "m": { "right": "not a name" } } }"#,
        ),
        (
            "an argument tag is unknown",
            r#"{ "doctype": "note", "methods": { "m": { "right": "read", "args": { "x": "gizmo" } } } }"#,
        ),
        (
            "a result tag is unknown",
            r#"{ "doctype": "note", "methods": { "m": { "right": "read", "result": { "x": "gizmo" } } } }"#,
        ),
    ];
    for (name, expr) in cases {
        let found = problems(expr);
        assert!(!found.is_empty(), "`{name}` produced no problem");
        assert!(
            found
                .iter()
                .all(|p| !p.code.is_empty() && !p.message.is_empty()),
            "`{name}` produced an empty problem"
        );
    }
}

#[test]
fn a_module_with_no_contract_export_is_refused() {
    let found = load_in_memory(
        "app",
        "controllers/m.ts",
        "export default {};",
        TsLimits::default(),
    )
    .err()
    .expect("no contract export");
    assert!(found.iter().any(|p| p.message.contains("contract")));
}

#[test]
fn a_source_that_does_not_transpile_reports_a_position() {
    let found = load_in_memory(
        "app",
        "controllers/m.ts",
        "export function contract( {",
        TsLimits::default(),
    )
    .err()
    .expect("a syntax error");
    assert!(found.iter().any(|p| p.position.is_some()));
    assert!(
        found
            .iter()
            .all(|p| p.code == spot_ts_controllers::COMPILE_FAILED)
    );
}
