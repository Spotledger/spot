//! The property tests: for every valid invocation, `parse(render(x)) == x` — and the
//! rendered string is the shell-quoted join of the argv the parser consumes. The parser
//! takes argv, so the property builds argv from the same flag rules and checks both.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use proptest::prelude::*;
use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, OpId, OperationSpec, ParamSpec, ParamType, ParamValue, Risk,
    Shell, TargetKind, argv, quote,
};

/// The awkward-text alphabet: every character a shell treats specially, plus plain text
/// and non-ASCII; every generated value is one the toy spec's types accept.
const PIECES: &[&str] = &[
    "nginx",
    "web app",
    "a'b",
    "c\"d",
    "dollar $x",
    "tick `y",
    "line\nbreak",
    "tab\there",
    "日本語",
    "semi;colon",
    "star*glob",
    "eq=sign",
    "brack[et]",
    "brace{y}",
    "pct%s",
    "at@host",
    "slash/path",
    "colon:time",
    "comma,list",
    "paren(x)",
    "amp&x",
    "pipe|x",
    "x",
    "",
    "__",
    "-e",
    "--",
];

/// One awkward text value: one to three pieces joined.
fn text_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(PIECES.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()),
        1..3,
    )
    .prop_map(|parts| parts.join(""))
}

/// A parameter spec of one of four types, paired with a value from its own domain.
fn param_strategy(prefix_root: &str) -> BoxedStrategy<(ParamSpec, ParamValue)> {
    let prefix = prefix_root.to_owned();
    (0usize..4, text_strategy(), -10_000i64..10_000)
        .prop_map(move |(kind, text, number)| {
            let name = format!("{prefix}{kind}");
            let (ty, value) = match kind {
                0 => (
                    ParamType::Text {
                        max_len: 512,
                        pattern: None,
                    },
                    ParamValue::Text(text),
                ),
                1 => (
                    ParamType::Int {
                        min: -10_000,
                        max: 10_000,
                    },
                    ParamValue::Int(number),
                ),
                2 => (
                    ParamType::List(Box::new(ParamType::Text {
                        max_len: 512,
                        pattern: None,
                    })),
                    ParamValue::List(vec![
                        ParamValue::Text(text),
                        ParamValue::Text("second'.elem".to_owned()),
                    ]),
                ),
                _ => (ParamType::Duration, ParamValue::Duration(7)),
            };
            (
                ParamSpec {
                    name,
                    ty,
                    required: true,
                    default: None,
                    doc: "A test parameter.".to_owned(),
                    secret: false,
                },
                value,
            )
        })
        .boxed()
}

/// A boolean parameter (the bare `--flag` / `--no-flag` forms).
fn bool_param_strategy(prefix_root: &str) -> BoxedStrategy<(ParamSpec, ParamValue)> {
    let prefix = prefix_root.to_owned();
    (0usize..4, proptest::bool::ANY)
        .prop_map(move |(kind, flag)| {
            (
                ParamSpec {
                    name: format!("{prefix}{kind}"),
                    ty: ParamType::Bool,
                    required: false,
                    default: None,
                    doc: "A test flag.".to_owned(),
                    secret: false,
                },
                ParamValue::Bool(flag),
            )
        })
        .boxed()
}

/// One toy operation with four generated parameters (sorted by name), plus the matching
/// (param, value) pairs in the same sorted order.
fn operation_strategy() -> BoxedStrategy<(OperationSpec, Vec<(ParamSpec, ParamValue)>)> {
    (
        param_strategy("alpha_"),
        param_strategy("beta_"),
        bool_param_strategy("gamma_"),
        bool_param_strategy("delta_"),
    )
        .prop_map(|(a, b, c, d)| {
            let mut pairs = vec![a, b, c, d];
            pairs.sort_by(|x, y| x.0.name.cmp(&y.0.name));
            let spec = OperationSpec {
                id: OpId::new("server.service.restart").unwrap(),
                title: "Round trip target".to_owned(),
                summary: "One operation the property tests drive.".to_owned(),
                params: pairs.iter().map(|(param, _)| param.clone()).collect(),
                risk: Risk::Medium,
                target: TargetKind::Server,
            };
            (spec, pairs)
        })
        .boxed()
}

/// Builds a one-operation catalogue (the shared `common::catalogue` has fixed parameters;
/// the property needs generated ones).
fn catalogue_of(specs: Vec<OperationSpec>) -> Catalogue {
    let mut builder = CatalogueBuilder::new();
    for spec in specs {
        builder.add("prop.module", spec);
    }
    builder.build().expect("the property's specs are valid")
}

/// The plain-text spelling of one value (render's own rule).
fn value_text(value: &ParamValue) -> String {
    match value {
        ParamValue::Bool(true) => "true".to_owned(),
        ParamValue::Bool(false) => "false".to_owned(),
        ParamValue::Int(number) => number.to_string(),
        ParamValue::Text(text) => text.clone(),
        ParamValue::Duration(seconds) => format!("{seconds}s"),
        ParamValue::List(items) => items.iter().map(value_text).collect::<Vec<_>>().join(","),
    }
}

/// Builds the argv the parser must read for the generated invocation, from the same flag
/// rules render follows: booleans bare or `--no-*`, lists repeating their flag.
fn expected_argv(values: &[(ParamSpec, ParamValue)]) -> Vec<String> {
    let mut out = vec![
        "spot".to_owned(),
        "ops".to_owned(),
        "server".to_owned(),
        "service".to_owned(),
        "restart".to_owned(),
    ];
    for (param, value) in values {
        let flag = param.name.replace('_', "-");
        match (&param.ty, value) {
            (ParamType::Bool, ParamValue::Bool(true)) => out.push(format!("--{flag}")),
            (ParamType::Bool, ParamValue::Bool(false)) => out.push(format!("--no-{flag}")),
            (ParamType::List(_), ParamValue::List(items)) => {
                for item in items {
                    out.push(format!("--{flag}"));
                    out.push(value_text(item));
                }
            }
            (_, value) => {
                out.push(format!("--{flag}"));
                out.push(value_text(value));
            }
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For every valid invocation: render's argv follows the flag rules, the rendered
    /// string is that argv's shell-quoted join, and the parser reads it back to the same
    /// typed parameters — for BOTH shells.
    #[test]
    fn parse_inverts_render(
        (spec, values) in operation_strategy(),
        shell in prop_oneof![Just(Shell::Posix), Just(Shell::PowerShell)],
    ) {
        let catalogue = catalogue_of(vec![spec]);
        let mut invocation = spot_mgmt_ops::Invocation::new(OpId::new("server.service.restart").unwrap());
        for (param, value) in &values {
            invocation.params.insert(param.name.clone(), value.clone());
        }
        let validated = catalogue
            .validate(&invocation)
            .expect("the property builds valid invocations");

        // The argv both sides agree on.
        let rendered_argv = argv(&validated);
        let expected = expected_argv(&values);
        prop_assert_eq!(&rendered_argv, &expected, "argv follows the flag rules");

        // The rendered string is that argv, shell-quoted and joined with spaces.
        let line = spot_mgmt_ops::render(&validated, shell);
        let joined = rendered_argv
            .iter()
            .map(|arg| quote(arg.as_str(), shell))
            .collect::<Vec<_>>()
            .join(" ");
        prop_assert_eq!(&line, &joined, "render is the shell-quoted join of argv");

        // The ROUND TRIP on the argv (what the parser takes).
        let reparsed = spot_mgmt_ops::parse::parse(&catalogue, &rendered_argv).expect("the argv parses");
        prop_assert_eq!(&reparsed, &invocation, "parse(ARGV) == x");

        // The rendered line's quoting must split back into exactly that argv: the inline
        // POSIX splitter mirrors the real `sh -c` test in parse_tests.rs.
        if shell == Shell::Posix {
            let split = posix_split(&line, false).expect("the rendered line splits to its argv");
            prop_assert_eq!(&split, &rendered_argv, "quoting round-trips through the splitter");
            let reparsed_split = spot_mgmt_ops::parse::parse(&catalogue, &split).expect("splits parse");
            prop_assert_eq!(&reparsed_split, &invocation, "parse(render-line argv) == x");
        } else {
            // PowerShell renders the same argv with its own quoting; split under that rule.
            let joined = rendered_argv
                .iter()
                .map(|arg| quote(arg.as_str(), shell))
                .collect::<Vec<_>>()
                .join(" ");
            prop_assert_eq!(&joined, &line, "the line was the PowerShell join");
            let split = posix_split(&line, true).expect("the PowerShell line splits");
            prop_assert_eq!(&split, &rendered_argv, "PowerShell quoting round-trips");
        }
    }

    /// Render is deterministic: the same validated invocation renders byte-identically,
    /// and a second parse-validate cycle of the parsed argv renders the same line when the
    /// invocation carries every parameter (no defaults in play).
    #[test]
    fn render_is_deterministic_per_shell(
        (spec, values) in operation_strategy(),
        shell in prop_oneof![Just(Shell::Posix), Just(Shell::PowerShell)],
    ) {
        let catalogue = catalogue_of(vec![spec]);
        let mut invocation = spot_mgmt_ops::Invocation::new(OpId::new("server.service.restart").unwrap());
        for (param, value) in &values {
            invocation.params.insert(param.name.clone(), value.clone());
        }
        let validated = catalogue.validate(&invocation).expect("valid");
        let once = spot_mgmt_ops::render(&validated, shell);
        let twice = spot_mgmt_ops::render(&validated, shell);
        prop_assert_eq!(&twice, &once, "byte-identical");
        // And through argv again: parse -> validate -> render is a fixed point.
        let rendered_argv = argv(&validated);
        let reparsed = spot_mgmt_ops::parse::parse(&catalogue, &rendered_argv).expect("parses");
        let revalidated = catalogue.validate(&reparsed).expect("valid");
        prop_assert_eq!(&spot_mgmt_ops::render(&revalidated, shell), &once, "fixed point");
    }
}

/// Splits a POSIX-rendered line back into its arguments, exactly the inverse of `quote`
/// (the real-shell test in `parse_tests.rs` proves the same thing through `sh -c`; this
/// proves it inline per property case). `power_shell = true` reads the PowerShell rule
/// instead: a doubled `'` inside quotes is one apostrophe.
fn posix_split(line: &str, power_shell: bool) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    let mut started = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '\'' => {
                    if power_shell && chars.peek() == Some(&'\'') {
                        // PowerShell: the doubled quote is one apostrophe.
                        chars.next();
                        current.push('\'');
                    } else {
                        in_quotes = false;
                    }
                }
                // POSIX: the '\'' escape is close, escaped quote, reopen — as three
                // characters the splitter reads as: closing `'`, literal `'`, opening `'`.
                '\\' if !power_shell && chars.peek() == Some(&'\'') => {
                    chars.next();
                    current.push('\'');
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                        // Still inside quotes? The escape closed before the quote and the
                        // trailing `'` reopens; this splitter is linear, so re-open.
                        in_quotes = true;
                    } else {
                        in_quotes = false;
                    }
                }
                other => current.push(other),
            }
            continue;
        }
        match c {
            ' ' => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '\'' => {
                in_quotes = true;
                started = true;
            }
            '\\' if !power_shell => {
                // A backslash outside quotes: the next character is literal.
                current.push(chars.next()?);
                started = true;
            }
            other => {
                current.push(other);
                started = true;
            }
        }
    }
    if in_quotes {
        return None;
    }
    if started {
        args.push(current);
    }
    Some(args)
}

/// The toy specs from `common` still hold their shape under the property build path
/// (restart, backup, user-create and the text echo the round-trip tests drive).
#[test]
fn the_shared_toy_catalogue_builds() {
    let catalogue = common::catalogue();
    assert_eq!(catalogue.len(), 4);
}

/// The `quote` function is the splitter's inverse for every awkward piece (spot checks
/// beyond the property: the `'` escape, the empty string, the newline).
#[test]
fn quote_and_split_agree_on_the_pieces() {
    for piece in PIECES {
        let posix_quoted = quote(piece, Shell::Posix);
        assert_eq!(
            posix_split(&posix_quoted, false),
            Some(vec![(*piece).to_owned()]),
            "posix: {posix_quoted:?}"
        );
        let pwsh_quoted = quote(piece, Shell::PowerShell);
        assert_eq!(
            posix_split(&pwsh_quoted, true),
            Some(vec![(*piece).to_owned()]),
            "powershell: {pwsh_quoted:?}"
        );
    }
    // The `'\''` POSIX escape splits back to one apostrophe.
    assert_eq!(
        posix_split("'a'\\''b'", false),
        Some(vec!["a'b".to_owned()])
    );
    // The doubled PowerShell quote splits back to one apostrophe under its own rule.
    assert_eq!(quote("a'b", Shell::PowerShell), "'a''b'");
    assert_eq!(posix_split("'a''b'", true), Some(vec!["a'b".to_owned()]));
}
