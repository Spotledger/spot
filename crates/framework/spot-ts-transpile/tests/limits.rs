//! Input limits: the size cap, the nesting-depth cap, and pathological inputs
//! that must be *rejected* (or finished) in bounded time rather than hang.
//! Time bounds are kept generous: the assertion is completion, not speed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;
use std::time::Instant;

use spot_ts_transpile::{Options, transpile};

fn finishes_within<F: FnOnce() -> R, R>(limit: Duration, what: &str, task: F) -> R {
    let started = Instant::now();
    let result = task();
    let spent = started.elapsed();
    assert!(
        spent < limit,
        "{what} took {spent:?}, over the {limit:?} budget"
    );
    result
}

#[test]
fn sources_over_the_cap_are_rejected_before_parsing() {
    let over = "const filler = 1;\n".repeat(200);
    let options = Options {
        source_size_cap: 512,
        ..Options::default()
    };
    let diagnostics = transpile(&over, "s.ts", &options).unwrap_err();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "ts.source_too_large");
    // Under the same cap a small source is accepted.
    transpile("export const one = 1;\n", "s.ts", &options).expect("under the cap");
}

#[test]
fn default_cap_is_one_mebibyte() {
    let options = Options::default();
    assert_eq!(options.source_size_cap, 1_048_576);
    let over = "const filler = true;\n".repeat(60_000); // ~1.2 MiB
    let diagnostics = finishes_within(Duration::from_secs(5), "1.2 MiB size check", || {
        transpile(&over, "s.ts", &options)
    })
    .unwrap_err();
    assert_eq!(diagnostics[0].code, "ts.source_too_large");
}

#[test]
fn nesting_over_the_cap_is_rejected_at_the_offending_bracket() {
    // 150 nested parentheses, cap 128: the 129th `(` is at byte offset 128,
    // which is line 1, column 129.
    let source = format!("{}{}", "((".repeat(75), "let x = 1;");
    let diagnostics = transpile(&source, "s.ts", &Options::default()).unwrap_err();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "ts.too_deep");
    assert_eq!((diagnostics[0].line, diagnostics[0].column), (1, 129));
    assert_eq!(
        (diagnostics[0].end_line, diagnostics[0].end_column),
        (1, 130)
    );
}

#[test]
fn nesting_in_templates_and_strings_is_measured_correctly() {
    // A harmless template literal with an escaped dollar and a string full of
    // brackets is accepted: the scanner skips string content.
    let source = "const note = `rate \\${x} and ((())) inside a string: '[[[' `;\n\nexport const ok = (): string => note;\n";
    transpile(source, "s.ts", &Options::default()).expect("string content is not nesting");
}

#[test]
fn pathological_inputs_reject_in_bounded_time() {
    // A megabyte of open parens: the pre-scan stops at the depth cap.
    let parens = "(".repeat(1_048_000);
    let diagnostics = finishes_within(Duration::from_secs(10), "paren flood", || {
        transpile(&parens, "s.ts", &Options::default())
    })
    .unwrap_err();
    assert_eq!(diagnostics[0].code, "ts.too_deep");

    // A sawtooth that repeatedly climbs close to the depth cap and back:
    // 100 climbs of 120-deep nesting — legal at the default cap, but heavy
    // enough that the linear scan (no regex, no recursion) is the thing that
    // must survive hostile size+shape; the parser then sees a large but
    // legitimate program (this is the wall-clock-free guard: the scanner
    // cannot hang on it, and neither can an exponential regex — there are none).
    let mut sawtooth = String::new();
    for _ in 0..100 {
        sawtooth.push_str(&"(".repeat(120));
        sawtooth.push('1');
        sawtooth.push_str(&")".repeat(120));
        sawtooth.push('\n');
    }
    sawtooth.push_str("export const ok = (): boolean => true;\n");
    let outcome = finishes_within(Duration::from_secs(5), "sawtooth nesting", || {
        transpile(&sawtooth, "s.ts", &Options::default())
    });
    // 120-deep nesting is under the 128 cap, so this parses and transpiles.
    let transpiled = outcome.expect("sawtooth under the cap is legal");
    assert!(transpiled.js.contains("ok"));
}

#[test]
fn hostile_whitespace_and_deep_templates_finish() {
    // Nested template substitutions: each `${` adds a frame; 100 of them are
    // legal at the default cap but heavy; the scanner must survive them.
    let mut nested = "export const note = ".to_string();
    for _ in 0..100 {
        nested.push_str("`x${");
    }
    nested.push('1');
    nested.push_str(&str::repeat("}`", 100));
    nested.push_str(";\n");
    let outcome = finishes_within(Duration::from_secs(5), "nested templates", || {
        transpile(&nested, "s.ts", &Options::default())
    });
    // `${` frames nest `{` depth too — 100 is under 128, so accepted or
    // rejected as a parse error; either verdict is a bounded outcome.
    drop(outcome);

    // Unterminated constructs of each kind, all bounded.
    let unterminated: &[String] = &[
        "const a = 'never ends".to_string(),
        "const a = `never ends".to_string(),
        "/* never ends".to_string(),
        "const re = /[a-".to_string(),
    ];
    for source in unterminated {
        let _ = finishes_within(Duration::from_secs(5), "unterminated construct", || {
            transpile(source, "s.ts", &Options::default())
        });
    }
}
