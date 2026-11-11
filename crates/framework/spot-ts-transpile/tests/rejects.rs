//! The reject rules: every construct the script tier forbids is reported with
//! its stable code and a 1-based position that points at the construct.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_ts_transpile::{Diagnostic, Options, transpile};

/// `true` when the diagnostic's start position lands on `needle` (the byte
/// range of one line of the source, 1-based addresses).
fn points_at(diagnostic: &Diagnostic, source: &str, needle: &str) -> bool {
    let line = source
        .lines()
        .nth(diagnostic.line as usize - 1)
        .unwrap_or("");
    let start = (diagnostic.column as usize - 1).min(line.len());
    line[start..].starts_with(needle)
}

fn codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect()
}

#[test]
fn imports_off_the_allow_list_are_rejected() {
    let out = transpile(
        "import { save } from \"lodash\";\n\nexport const run = (): void => save({});\n",
        "s.ts",
        &Options::default(),
    );
    let diagnostics = out.unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.import_not_allowed"]);
    assert!(points_at(
        &diagnostics[0],
        "import { save } from \"lodash\";",
        "import"
    ));
}

#[test]
fn a_wildcard_allow_list_replaces_the_default() {
    let source = "import { one } from \"shop:items\";\n\nexport const label = (): string => one;\n";
    let strict = transpile(source, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&strict), vec!["ts.import_not_allowed"]);

    let options = Options {
        allow_imports: vec!["spot:*".to_string(), "shop:*".to_string()],
        ..Options::default()
    };
    let allowed = transpile(source, "s.ts", &options).expect("shop:* now allowed");
    assert!(allowed.js.contains("shop:items"));
}

#[test]
fn re_exports_hit_the_allow_list_too() {
    let from = transpile(
        "export { parse } from \"papaparse\";\n\nexport const ok = (): boolean => true;\n",
        "s.ts",
        &Options::default(),
    );
    assert_eq!(codes(&from.unwrap_err()), vec!["ts.import_not_allowed"]);

    let all = transpile(
        "export * from \"left-pad\";\n\nexport const ok = (): boolean => true;\n",
        "s.ts",
        &Options::default(),
    );
    assert_eq!(codes(&all.unwrap_err()), vec!["ts.import_not_allowed"]);
}

#[test]
fn dynamic_import_is_rejected() {
    let source =
        "export const loader = async (): Promise<unknown> => import(\"spot:documents\");\n";
    let diagnostics = transpile(source, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.dynamic_import_not_allowed"]);
    assert!(points_at(&diagnostics[0], source, "import"));
}

#[test]
fn eval_is_rejected_wherever_it_is_referred_to() {
    let call = "export const boom = (code: string): unknown => eval(code);\n";
    let diagnostics = transpile(call, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.eval_not_allowed"]);
    assert!(points_at(&diagnostics[0], call, "eval"));

    let bare =
        "const alias = eval;\n\nexport const boom = (code: string): unknown => alias(code);\n";
    let diagnostics = transpile(bare, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.eval_not_allowed"]);
    assert!(points_at(&diagnostics[0], bare, "eval"));
}

#[test]
fn function_constructor_forms_are_rejected() {
    let constructor =
        "export const factory = (code: string): unknown => new Function(\"x\", code);\n";
    let diagnostics = transpile(constructor, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(
        codes(&diagnostics),
        vec!["ts.function_constructor_not_allowed"]
    );
    assert!(points_at(&diagnostics[0], constructor, "new Function"));

    let call = "export const factory = (code: string): unknown => Function(\"x\", code);\n";
    let diagnostics = transpile(call, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(
        codes(&diagnostics),
        vec!["ts.function_constructor_not_allowed"]
    );
    assert!(points_at(&diagnostics[0], call, "Function"));
}

#[test]
fn require_forms_are_rejected() {
    let call = "export const legacy = (): unknown => require(\"spot:documents\");\n";
    let diagnostics = transpile(call, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.require_not_allowed"]);
    assert!(points_at(&diagnostics[0], call, "require"));

    let equals =
        "import docs = require(\"spot:documents\");\n\nexport const first = (): unknown => docs;\n";
    let diagnostics = transpile(equals, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.require_not_allowed"]);
    assert!(points_at(&diagnostics[0], equals, "import docs"));
}

#[test]
fn with_statements_are_rejected() {
    let source = "const scope = { name: \"doc\" };\n\nexport const read = (): string => {\n  with (scope) {\n    return scope.name;\n  }\n};\n";
    let diagnostics = transpile(source, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.with_not_allowed"]);
    assert!(points_at(&diagnostics[0], source, "with"));
}

#[test]
fn decorators_are_rejected_with_their_position() {
    let class_level = "function frozen(target: unknown): unknown {\n  return target;\n}\n\n@frozen\nexport class Report {\n  readonly id = \"r1\";\n}\n";
    let diagnostics = transpile(class_level, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.decorator_not_supported"]);
    assert_eq!((diagnostics[0].line, diagnostics[0].column), (5, 1));
    assert!(points_at(&diagnostics[0], class_level, "@frozen"));

    let method_level = "function logged(target: unknown): void {}\n\nexport class Runner {\n  @logged\n  start(): void {}\n}\n";
    let diagnostics = transpile(method_level, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.decorator_not_supported"]);
    assert!(points_at(&diagnostics[0], method_level, "@logged"));
}

#[test]
fn namespaces_are_rejected_in_all_spellings() {
    let namespace = "namespace Util {\n  export const id = (): number => 1;\n}\n\nexport const use = (): number => Util.id();\n";
    let diagnostics = transpile(namespace, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.namespace_not_supported"]);
    assert!(points_at(&diagnostics[0], namespace, "namespace"));

    let module = "module Util {\n  export const id = (): number => 1;\n}\n\nexport const use = (): number => Util.id();\n";
    let diagnostics = transpile(module, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(codes(&diagnostics), vec!["ts.namespace_not_supported"]);
    assert!(points_at(&diagnostics[0], module, "module"));

    let import_equals = "import legacy = Util;\n\nnamespace Util {\n  export const id = (): number => 1;\n}\n\nexport const use = (): number => legacy.id();\n";
    let diagnostics = transpile(import_equals, "s.ts", &Options::default()).unwrap_err();
    // Both problems: the `import legacy = Util` and the `namespace Util`.
    assert_eq!(
        codes(&diagnostics),
        vec!["ts.namespace_not_supported", "ts.namespace_not_supported"]
    );
    assert!(points_at(&diagnostics[0], import_equals, "import legacy"));
    assert!(points_at(&diagnostics[1], import_equals, "namespace"));
}

#[test]
fn syntax_errors_report_every_error_the_parser_recovers() {
    let source = "const a = ;\nconst b = ;\n\nexport const sum = (): number => a + b;\n";
    let diagnostics = transpile(source, "s.ts", &Options::default()).unwrap_err();
    assert!(
        !diagnostics.is_empty(),
        "at least one parse problem reported"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code == "ts.syntax")
    );
    for diagnostic in &diagnostics {
        assert!(
            diagnostic.line >= 1 && diagnostic.column >= 1,
            "1-based positions"
        );
        assert!(
            diagnostic.end_line > diagnostic.line
                || (diagnostic.end_line == diagnostic.line
                    && diagnostic.end_column >= diagnostic.column),
            "end position at or after the start",
        );
    }
    // The first reported error sits on the first broken line, pointing into it.
    assert_eq!(diagnostics[0].line, 1);
    assert!(points_at(&diagnostics[0], source, ";"));
}

#[test]
fn every_diagnostic_position_is_1_based_and_inside_the_source() {
    let sources: &[&str] = &[
        "import x from \"nope\";\nexport const y = (): number => x;\n",
        "export const boom = (): unknown => eval(\"1\");\n",
        "const a = ;\n",
        "import docs = require(\"spot:documents\");\n",
        "namespace U { export const a = 1; }\n",
        "@frozen\nexport class R { readonly id = \"r1\"; }\n",
        "export const loader = (): unknown => import(\"spot:x\");\n",
    ];
    for source in sources {
        let diagnostics = transpile(source, "s.ts", &Options::default()).unwrap_err();
        for diagnostic in &diagnostics {
            assert!(diagnostic.line >= 1, "start line 1-based");
            assert!(diagnostic.column >= 1, "start column 1-based");
            assert!(diagnostic.end_line >= 1, "end line 1-based");
            assert!(diagnostic.end_column >= 1, "end column 1-based");
        }
    }
}
