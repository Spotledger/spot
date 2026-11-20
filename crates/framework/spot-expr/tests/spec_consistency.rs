//! The specification and the implementation cannot drift (WP-UI-11.T3).
//!
//! `docs/expression-language.md` is the document the .NET implementation is written against.
//! The conformance corpus proves behaviour for the cases it lists; this file treats the
//! document as data and fails when its function table, its limits, its error codes or its
//! operator list disagree with the library: a function added to one and not the other, a limit
//! changed in one place, a code named in the document that the catalogue does not hold.
//!
//! The document is read with `str` methods only. Every check asserts a lower bound on what it
//! found, so a change to the document's layout fails loudly ("found 0 function rows") instead
//! of silently checking nothing.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use spot_expr::{MAX_DEPTH, MAX_NODES, codes, parse};

const FUNCTIONS_RS: &str = include_str!("../src/functions.rs");
const ERROR_RS: &str = include_str!("../src/error.rs");

/// A disagreement between the document and the library that the lead has agreed to fix later:
/// (test name, subject, why). A disagreement found by a test is listed here, never fixed in this
/// file, because neither the document nor the library is owned by this task. Empty: there is none.
const KNOWN_DRIFT: &[(&str, &str, &str)] = &[];

/// Functions whose arity is not a plain count: (name, fewest arguments accepted, a phrase that the
/// document's row for the function must contain, which is where the document states the rule).
/// The signature of such a row ends in `...`, so the count of named arguments is not the rule:
/// `coalesce(a, b, ...)` names two arguments and the rule text says "at least one argument".
const VARIADIC: &[(&str, usize, &str)] = &[
    ("coalesce", 1, "at least one argument"),
    ("min", 2, "at least two arguments"),
    ("max", 2, "as `min`"),
];

/// Library functions that are deliberately not in the document. Expected empty.
const INTERNAL: &[(&str, &str)] = &[];

/// Codes of the catalogue that the document does not need to mention. Expected empty.
const UNDOCUMENTED_OK: &[(&str, &str)] = &[];

/// Every constant of `spot_expr::codes`. The test below proves this list is complete by reading
/// `src/error.rs`, so a new constant has to be added here.
const ALL_CODES: &[&str] = &[
    codes::SYNTAX,
    codes::UNTERMINATED_STRING,
    codes::BAD_ESCAPE,
    codes::NUMBER_RANGE,
    codes::TOO_COMPLEX,
    codes::TOO_DEEP,
    codes::UNKNOWN_NAME,
    codes::UNKNOWN_FUNCTION,
    codes::ARITY,
    codes::TYPE,
    codes::DIVISION_BY_ZERO,
    codes::OVERFLOW,
    codes::BAD_DATE,
    codes::BAD_NUMBER,
];

/// One minimal expression per documented operator token (the precedence table of section 3).
const SAMPLES: &[(&str, &str)] = &[
    ("?", "true ? 1 : 2"),
    (":", "true ? 1 : 2"),
    ("or", "true or false"),
    ("and", "true and false"),
    ("not", "not true"),
    ("==", "1 == 1"),
    ("!=", "1 != 2"),
    ("<", "1 < 2"),
    ("<=", "1 <= 2"),
    (">", "2 > 1"),
    (">=", "2 >= 1"),
    ("in", "1 in [1]"),
    ("+", "1 + 1"),
    ("-", "2 - 1"),
    ("*", "2 * 3"),
    ("/", "6 / 3"),
    ("%", "7 % 3"),
];

// ------------------------------------------------------------------------------------------
// Reading the files.
// ------------------------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn read(path: PathBuf) -> String {
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn document() -> String {
    read(repo_root().join("docs/expression-language.md"))
}

fn catalogue() -> String {
    read(repo_root().join("crates/framework/spot-errors/catalogue/expr.toml"))
}

/// Records a failure unless the disagreement is in `KNOWN_DRIFT`.
fn record(failures: &mut Vec<String>, test: &str, subject: &str, message: String) {
    if !KNOWN_DRIFT
        .iter()
        .any(|(t, s, _)| *t == test && *s == subject)
    {
        failures.push(message);
    }
}

fn finish(test: &str, failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{test}: {} disagreement(s) between docs/expression-language.md and the library:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

// ------------------------------------------------------------------------------------------
// The Markdown, by hand.
// ------------------------------------------------------------------------------------------

/// The cells of a table row. A `|` inside a backtick span, or written `\|`, does not split.
fn split_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_code = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                in_code = !in_code;
                cell.push(c);
            }
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' if !in_code => {
                cells.push(cell.trim().to_owned());
                cell.clear();
            }
            _ => cell.push(c),
        }
    }
    cells.push(cell.trim().to_owned());
    cells
}

/// One row of the section 8 table.
#[derive(Debug, PartialEq)]
struct Row {
    /// 1-based line in the document.
    line: usize,
    name: String,
    args: Vec<String>,
    /// The result and rule cells joined, where the document states the arity rule.
    text: String,
}

/// The rows of the function table of section 8: lines whose first cell is a backticked call.
fn function_rows(doc: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut in_section = false;
    for (index, line) in doc.lines().enumerate() {
        if line.starts_with("## ") {
            in_section = line.starts_with("## 8.");
            continue;
        }
        if !in_section || !line.trim_start().starts_with('|') {
            continue;
        }
        let cells = split_cells(line);
        if cells.len() < 3 {
            continue;
        }
        let Some(call) = cells[0].strip_prefix('`').and_then(|c| c.strip_suffix('`')) else {
            continue;
        };
        let Some(open) = call.find('(') else {
            continue;
        };
        let Some(inner) = call[open + 1..].strip_suffix(')') else {
            continue;
        };
        let args = if inner.trim().is_empty() {
            Vec::new()
        } else {
            inner.split(',').map(|a| a.trim().to_owned()).collect()
        };
        rows.push(Row {
            line: index + 1,
            name: call[..open].to_owned(),
            args,
            text: cells[1..].join(" | "),
        });
    }
    rows
}

/// The `expr.<name>` tokens of the document with their lines.
fn doc_codes(doc: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (index, line) in doc.lines().enumerate() {
        let mut from = 0;
        while let Some(pos) = line[from..].find("expr.") {
            let at = from + pos;
            let before_ok = line[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'));
            let name: String = line[at + 5..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '_')
                .collect();
            if before_ok && !name.is_empty() {
                found.push((index + 1, format!("expr.{name}")));
            }
            from = at + 5;
        }
    }
    found
}

/// The operator tokens of the precedence table: rows whose first cell is a level number, from
/// the code spans of the second cell split on whitespace (`` `== != < <= > >= in` ``).
fn operator_tokens(doc: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (index, line) in doc.lines().enumerate() {
        if !line.trim_start().starts_with('|') {
            continue;
        }
        let cells = split_cells(line);
        if cells.len() < 3 || cells[0].is_empty() || !cells[0].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        for (n, segment) in cells[1].split('`').enumerate() {
            if n % 2 == 1 {
                for token in segment.split_whitespace() {
                    found.push((index + 1, token.to_owned()));
                }
            }
        }
    }
    found
}

/// The number after "at most" on the document line that names `label`.
fn documented_limit(doc: &str, label: &str) -> Option<(usize, usize)> {
    for (index, line) in doc.lines().enumerate() {
        if line.contains(label) {
            let after = line.split("at most").nth(1)?;
            let digits: String = after
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            return digits.parse().ok().map(|n| (index + 1, n));
        }
    }
    None
}

/// The string literals of the match arms of `fn arity` in `functions.rs`.
fn library_functions() -> Vec<String> {
    let start = FUNCTIONS_RS
        .find("fn arity(")
        .expect("functions.rs has `fn arity(`");
    let rest = &FUNCTIONS_RS[start..];
    let end = rest
        .find("_ => return None")
        .expect("`fn arity` ends in `_ => return None`");
    rest[..end]
        .split('"')
        .enumerate()
        .filter(|(n, _)| n % 2 == 1)
        .map(|(_, name)| name.to_owned())
        .collect()
}

fn call_text(name: &str, count: usize) -> String {
    format!("{name}({})", vec!["null"; count].join(", "))
}

// ------------------------------------------------------------------------------------------
// The checks.
// ------------------------------------------------------------------------------------------

#[test]
fn every_documented_function_exists_with_the_documented_arity() {
    const TEST: &str = "every_documented_function_exists_with_the_documented_arity";
    let doc = document();
    let rows = function_rows(&doc);
    assert!(
        rows.len() >= 20,
        "found {} function rows in section 8 of the document, expected at least 20: did the table change shape?",
        rows.len()
    );
    let mut failures = Vec::new();
    for row in &rows {
        let at = format!("doc line {}, `{}`", row.line, row.name);
        let ellipsis = row.args.iter().any(|a| a == "...");
        let variadic = VARIADIC.iter().find(|(name, _, _)| *name == row.name);
        let (count, exact) = match variadic {
            Some((_, fewest, phrase)) => {
                if !ellipsis {
                    record(
                        &mut failures,
                        TEST,
                        &row.name,
                        format!("{at}: listed in VARIADIC but its signature has no `...`"),
                    );
                }
                if !row.text.contains(phrase) {
                    record(
                        &mut failures,
                        TEST,
                        &row.name,
                        format!(
                            "{at}: the row no longer says \"{phrase}\", which is the rule VARIADIC encodes"
                        ),
                    );
                }
                (*fewest, false)
            }
            None => {
                if ellipsis {
                    record(
                        &mut failures,
                        TEST,
                        &row.name,
                        format!(
                            "{at}: the signature has `...` but the function is not in VARIADIC; add it with the rule's count and phrase"
                        ),
                    );
                    continue;
                }
                (row.args.len(), true)
            }
        };

        let accepted = call_text(&row.name, count);
        if let Err(e) = parse(&accepted) {
            record(
                &mut failures,
                TEST,
                &row.name,
                format!(
                    "{at}: `{accepted}` is refused by the library ({e}), the document accepts it"
                ),
            );
            continue;
        }
        if count > 0 {
            let fewer = call_text(&row.name, count - 1);
            match parse(&fewer) {
                Err(e) if e.code == codes::ARITY => {}
                Err(e) => record(
                    &mut failures,
                    TEST,
                    &row.name,
                    format!(
                        "{at}: `{fewer}` is `{}`, the document implies `{}`",
                        e.code,
                        codes::ARITY
                    ),
                ),
                Ok(_) => record(
                    &mut failures,
                    TEST,
                    &row.name,
                    format!(
                        "{at}: `{fewer}` parses, the document says fewer than {count} arguments is `{}`",
                        codes::ARITY
                    ),
                ),
            }
        }
        let more = call_text(&row.name, if exact { count + 1 } else { count + 3 });
        match (exact, parse(&more)) {
            (true, Err(e)) if e.code == codes::ARITY => {}
            (true, Err(e)) => record(
                &mut failures,
                TEST,
                &row.name,
                format!(
                    "{at}: `{more}` is `{}`, the document implies `{}`",
                    e.code,
                    codes::ARITY
                ),
            ),
            (true, Ok(_)) => record(
                &mut failures,
                TEST,
                &row.name,
                format!(
                    "{at}: `{more}` parses, the document gives `{}` exactly {count} arguments",
                    row.name
                ),
            ),
            (false, Ok(_)) => {}
            (false, Err(e)) => record(
                &mut failures,
                TEST,
                &row.name,
                format!(
                    "{at}: `{more}` is refused ({e}), the document says any number from {count} up is accepted"
                ),
            ),
        }

        let unknown = call_text(&format!("{}x", row.name), 1);
        match parse(&unknown) {
            Err(e) if e.code == codes::UNKNOWN_FUNCTION => {}
            other => record(
                &mut failures,
                TEST,
                &row.name,
                format!(
                    "{at}: `{unknown}` should be `{}`, got {:?}",
                    codes::UNKNOWN_FUNCTION,
                    other.map(|_| "a parse")
                ),
            ),
        }
    }
    finish(TEST, &failures);
}

#[test]
fn every_library_function_is_documented() {
    const TEST: &str = "every_library_function_is_documented";
    let doc = document();
    let documented: BTreeSet<String> = function_rows(&doc).into_iter().map(|r| r.name).collect();
    let library = library_functions();
    assert!(
        library.len() >= 20,
        "found {} function names in `fn arity` of functions.rs, expected at least 20: did the match change shape?",
        library.len()
    );
    let mut failures = Vec::new();
    for name in &library {
        if !documented.contains(name) && !INTERNAL.iter().any(|(n, _)| *n == name.as_str()) {
            record(
                &mut failures,
                TEST,
                name,
                format!(
                    "`{name}` is in `fn arity` (functions.rs) but has no row in section 8 of the document"
                ),
            );
        }
    }
    let known: BTreeSet<&str> = library.iter().map(String::as_str).collect();
    for name in &documented {
        if !known.contains(name.as_str()) {
            record(
                &mut failures,
                TEST,
                name,
                format!(
                    "`{name}` has a row in the document but is not in `fn arity` (functions.rs)"
                ),
            );
        }
    }
    finish(TEST, &failures);
}

#[test]
fn the_documented_limits_are_the_library_limits() {
    const TEST: &str = "the_documented_limits_are_the_library_limits";
    let doc = document();
    let nodes = documented_limit(&doc, "Node count");
    let depth = documented_limit(&doc, "Nesting depth");
    let mut failures = Vec::new();
    match nodes {
        Some((_, n)) if n == MAX_NODES => {}
        Some((line, n)) => record(
            &mut failures,
            TEST,
            "node count",
            format!("doc line {line}: the node limit is {n}, MAX_NODES is {MAX_NODES}"),
        ),
        None => failures.push("found no \"Node count: at most N\" line in the document".to_owned()),
    }
    match depth {
        Some((_, n)) if n == MAX_DEPTH => {}
        Some((line, n)) => record(
            &mut failures,
            TEST,
            "nesting depth",
            format!("doc line {line}: the depth limit is {n}, MAX_DEPTH is {MAX_DEPTH}"),
        ),
        None => {
            failures.push("found no \"Nesting depth: at most N\" line in the document".to_owned())
        }
    }

    // Nodes: a list of k literals has k + 1 nodes and depth 2.
    let list_of = |k: usize| format!("[{}]", vec!["1"; k].join(", "));
    if let Err(e) = parse(&list_of(MAX_NODES - 1)) {
        failures.push(format!(
            "an expression of exactly MAX_NODES ({MAX_NODES}) nodes is refused: {e}"
        ));
    }
    match parse(&list_of(MAX_NODES)) {
        Err(e) if e.code == codes::TOO_COMPLEX => {}
        other => failures.push(format!(
            "an expression of MAX_NODES + 1 nodes should be `{}`, got {:?}",
            codes::TOO_COMPLEX,
            other.map(|_| "a parse").map_err(|e| e.code)
        )),
    }

    // Depth: a left-deep chain `1 + 1 + ... + 1` of n terms has depth n (the document says so).
    let chain_of = |terms: usize| vec!["1"; terms].join(" + ");
    if let Err(e) = parse(&chain_of(MAX_DEPTH)) {
        failures.push(format!(
            "a chain of exactly MAX_DEPTH ({MAX_DEPTH}) terms is refused: {e}"
        ));
    }
    match parse(&chain_of(MAX_DEPTH + 1)) {
        Err(e) if e.code == codes::TOO_DEEP => {}
        other => failures.push(format!(
            "a chain of MAX_DEPTH + 1 terms should be `{}`, got {:?}",
            codes::TOO_DEEP,
            other.map(|_| "a parse").map_err(|e| e.code)
        )),
    }
    finish(TEST, &failures);
}

#[test]
fn every_documented_error_code_is_registered_and_used() {
    const TEST: &str = "every_documented_error_code_is_registered_and_used";
    let doc = document();
    let toml = catalogue();
    let documented = doc_codes(&doc);
    let registered: BTreeSet<String> = toml
        .lines()
        .filter_map(|line| line.trim().strip_prefix("code = \""))
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_owned)
        .collect();
    assert!(
        documented.len() >= 5,
        "found {} `expr.*` codes in the document, expected at least 5: did the layout change?",
        documented.len()
    );
    assert!(
        registered.len() >= 5,
        "found {} codes in expr.toml, expected at least 5: is the key still `code = \"...\"`?",
        registered.len()
    );

    let mut failures = Vec::new();
    // The list ALL_CODES is complete: every `expr.*` literal of error.rs is in it, and the reverse.
    let in_source: BTreeSet<String> = ERROR_RS
        .split('"')
        .enumerate()
        .filter(|(n, text)| n % 2 == 1 && text.starts_with("expr."))
        .map(|(_, text)| text.to_owned())
        .collect();
    let listed: BTreeSet<String> = ALL_CODES.iter().map(|c| (*c).to_owned()).collect();
    for code in in_source.difference(&listed) {
        failures.push(format!(
            "`{code}` is a constant of src/error.rs but is missing from ALL_CODES in this test"
        ));
    }
    for code in listed.difference(&in_source) {
        failures.push(format!(
            "`{code}` is in ALL_CODES but is not a constant of src/error.rs"
        ));
    }

    for (line, code) in &documented {
        if !listed.contains(code) {
            record(
                &mut failures,
                TEST,
                code,
                format!("doc line {line}: `{code}` is not a constant of spot_expr::codes"),
            );
        }
        if !registered.contains(code) {
            record(
                &mut failures,
                TEST,
                code,
                format!("doc line {line}: `{code}` is not registered in catalogue/expr.toml"),
            );
        }
    }
    let mentioned: BTreeSet<&str> = documented.iter().map(|(_, c)| c.as_str()).collect();
    for code in &registered {
        if !mentioned.contains(code.as_str())
            && !UNDOCUMENTED_OK.iter().any(|(c, _)| *c == code.as_str())
        {
            record(
                &mut failures,
                TEST,
                code,
                format!("`{code}` is in catalogue/expr.toml but the document never mentions it"),
            );
        }
    }
    for code in &listed {
        if !registered.contains(code) {
            record(
                &mut failures,
                TEST,
                code,
                format!(
                    "`{code}` is a constant of spot_expr::codes but is not registered in catalogue/expr.toml"
                ),
            );
        }
    }
    finish(TEST, &failures);
}

#[test]
fn the_documented_operators_all_lex_and_parse() {
    const TEST: &str = "the_documented_operators_all_lex_and_parse";
    let doc = document();
    let tokens = operator_tokens(&doc);
    let distinct: BTreeSet<&str> = tokens.iter().map(|(_, t)| t.as_str()).collect();
    assert!(
        distinct.len() >= 8,
        "found {} operator tokens in the precedence table, expected at least 8: did the table change shape? ({distinct:?})",
        distinct.len()
    );
    let mut failures = Vec::new();
    for (line, token) in &tokens {
        match SAMPLES.iter().find(|(t, _)| *t == token.as_str()) {
            None => record(
                &mut failures,
                TEST,
                token,
                format!(
                    "doc line {line}: the operator `{token}` has no sample in SAMPLES; add one"
                ),
            ),
            Some((_, source)) => match parse(source) {
                Err(e) => record(
                    &mut failures,
                    TEST,
                    token,
                    format!(
                        "doc line {line}: `{source}` (operator `{token}`) is refused by the library: {e}"
                    ),
                ),
                Ok(expr) => {
                    let printed = expr.to_string();
                    if parse(&printed) != Ok(expr.clone()) {
                        record(
                            &mut failures,
                            TEST,
                            token,
                            format!(
                                "doc line {line}: `{source}` printed as `{printed}` does not parse back to the same tree"
                            ),
                        );
                    }
                }
            },
        }
    }
    // Unary minus, and the two statements of the precedence table's worked sentence.
    let equal = |a: &str, b: &str| parse(a).is_ok() && parse(a) == parse(b);
    for (a, b, why) in [
        ("-1", "-1", "unary minus parses"),
        (
            "not 1 == 2",
            "not (1 == 2)",
            "`not a == b` is `not (a == b)`",
        ),
        ("-1 * 2", "(-1) * 2", "`-a * b` is `(-a) * b`"),
        (
            "true or false and false",
            "true or (false and false)",
            "`and` binds tighter than `or`",
        ),
        ("1 + 2 * 3", "1 + (2 * 3)", "`*` binds tighter than `+`"),
        ("1 - 2 - 3", "(1 - 2) - 3", "`+ -` is left associative"),
    ] {
        if !equal(a, b) {
            failures.push(format!(
                "`{a}` and `{b}` should parse to the same tree ({why})"
            ));
        }
    }
    match parse("1 < 2 < 3") {
        Err(e) if e.code == codes::SYNTAX => {}
        other => failures.push(format!(
            "`1 < 2 < 3` should be `{}` (comparison does not associate), got {:?}",
            codes::SYNTAX,
            other.map(|_| "a parse").map_err(|e| e.code)
        )),
    }
    match parse("[1, 2,]") {
        Err(e) if e.code == codes::SYNTAX => {}
        other => failures.push(format!(
            "a trailing comma should be `{}`, got {:?}",
            codes::SYNTAX,
            other.map(|_| "a parse").map_err(|e| e.code)
        )),
    }
    finish(TEST, &failures);
}

// ------------------------------------------------------------------------------------------
// The Markdown reading is itself checked.
// ------------------------------------------------------------------------------------------

#[test]
fn the_document_table_parser_itself_is_checked() {
    const SECTION: &str = "\
# Title

## 7. Dates

| `not_a_function(x)` | `text` | outside section 8 |

## 8. Function library

| Function | Result | Rule |
| --- | --- | --- |
| `len(x)` | `int` | length |
| `pick(a | b)` | `text` | a pipe inside backticks |
| `join(a, b, ...)` | any | at least one argument; the `a | b` form |
| `now()` | `datetime` | no arguments |
| plain text | `int` | no backticked first cell |
| `broken` | `int` | no parentheses |

## 9. Errors

| `late(x)` | `int` | after section 8 |
";
    let rows = function_rows(SECTION);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["len", "pick", "join", "now"]);
    assert_eq!(rows[0].args, ["x"]);
    assert_eq!(rows[1].args, ["a | b"]);
    assert_eq!(rows[2].args, ["a", "b", "..."]);
    assert!(rows[3].args.is_empty());
    assert!(rows[2].text.contains("at least one argument"));
    assert!(rows[2].text.contains("`a | b`"));
    assert_eq!(rows[0].line, 11);
    assert!(
        function_rows("no table here").is_empty(),
        "no rows, not a panic"
    );
}

#[test]
fn cells_split_outside_backticks_only() {
    assert_eq!(split_cells("| a | `b | c` | d |"), ["a", "`b | c`", "d"]);
    assert_eq!(split_cells("| a \\| b | c |"), ["a | b", "c"]);
    assert_eq!(split_cells("|  |  |"), ["", ""]);
}

#[test]
fn operator_tokens_come_from_the_code_spans_of_the_second_cell() {
    const TABLE: &str = "\
| Level | Operators | Associativity |
| --- | --- | --- |
| 1 | `? :` | right |
| 5 | `== != < <= > >= in` | none |
| 8 | unary `-` | prefix |
| not a level | `x` | none |
";
    let tokens: Vec<String> = operator_tokens(TABLE).into_iter().map(|(_, t)| t).collect();
    assert_eq!(
        tokens,
        ["?", ":", "==", "!=", "<", "<=", ">", ">=", "in", "-"]
    );
}

#[test]
fn error_code_tokens_are_found_with_their_lines() {
    let doc = "intro\n| `expr.syntax` | when |\nspot-expr.x is not a code, nor is xexpr.y\nand `expr.too_deep`, `expr.arity`.\n";
    assert_eq!(
        doc_codes(doc),
        [
            (2, "expr.syntax".to_owned()),
            (4, "expr.too_deep".to_owned()),
            (4, "expr.arity".to_owned())
        ]
    );
}

#[test]
fn limits_are_read_from_the_line_that_names_them() {
    let doc = "- **Node count**: at most **2000** nodes. More is `expr.too_complex`.\n- **Nesting depth**: at most **64**. Depth is 1.\n";
    assert_eq!(documented_limit(doc, "Node count"), Some((1, 2000)));
    assert_eq!(documented_limit(doc, "Nesting depth"), Some((2, 64)));
    assert_eq!(documented_limit(doc, "Absent"), None);
    assert_eq!(
        documented_limit("Node count without a number", "Node count"),
        None
    );
}

#[test]
fn the_library_function_names_are_found_in_functions_rs() {
    let names = library_functions();
    assert!(names.iter().any(|n| n == "len"), "{names:?}");
    assert!(names.iter().any(|n| n == "coalesce"), "{names:?}");
    assert!(names.iter().any(|n| n == "now"), "{names:?}");
    assert!(
        !names.iter().any(|n| n.contains(' ') || n.is_empty()),
        "{names:?}"
    );
}
