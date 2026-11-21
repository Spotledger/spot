//! Property tests: printing and parsing agree, evaluation never panics, and decimal results
//! match an independent scaled-integer oracle.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use proptest::prelude::*;
use rust_decimal::Decimal;
use spot_expr::{BinaryOp, DateTime, Expr, MapContext, Root, UnaryOp, Value, parse};

/// Functions with an arity that suits a generator: (name, arguments).
const CALLS: &[(&str, usize)] = &[
    ("len", 1),
    ("lower", 1),
    ("trim", 1),
    ("is_empty", 1),
    ("abs", 1),
    ("text", 1),
    ("int", 1),
    ("decimal", 1),
    ("date", 1),
    ("contains", 2),
    ("starts_with", 2),
    ("round", 2),
    ("date_add", 2),
    ("days_between", 2),
    ("substr", 3),
    ("replace", 3),
    ("coalesce", 2),
    ("min", 2),
    ("max", 3),
    ("now", 0),
];

fn decimal() -> impl Strategy<Value = Decimal> {
    (0_i64..1_000_000_000_000, 1_u32..=12)
        .prop_map(|(m, s)| Decimal::from_i128_with_scale(i128::from(m), s))
}

/// A path segment strategy: an IDENT, never a keyword. The lexer reserves and/or/not/in/
/// true/false/null as their own token classes, so a keyword segment (`doc.or`) is a parse
/// error by the grammar (`path <- ("doc" / "parent") ("." IDENT)+`), and printing such an
/// AST could not round-trip.
fn segment() -> impl Strategy<Value = String> {
    "[a-z_][a-z0-9_]{0,5}".prop_filter("not a keyword", |s: &String| {
        !matches!(
            s.as_str(),
            "and" | "or" | "not" | "in" | "true" | "false" | "null"
        )
    })
}

fn leaf() -> impl Strategy<Value = Expr> {
    prop_oneof![
        Just(Expr::Null),
        any::<bool>().prop_map(Expr::Bool),
        (0_i64..=i64::MAX).prop_map(Expr::Int),
        decimal().prop_map(Expr::Decimal),
        "[ -~\\n\\t\\u{e9}\\u{65e5}]{0,8}".prop_map(Expr::Text),
        proptest::collection::vec(segment(), 1..3).prop_map(|p| Expr::Path(Root::Doc, p)),
        proptest::collection::vec(segment(), 1..3).prop_map(|p| Expr::Path(Root::Parent, p)),
    ]
}

fn binary_op() -> impl Strategy<Value = BinaryOp> {
    prop_oneof![
        Just(BinaryOp::Or),
        Just(BinaryOp::And),
        Just(BinaryOp::Eq),
        Just(BinaryOp::Ne),
        Just(BinaryOp::Lt),
        Just(BinaryOp::Le),
        Just(BinaryOp::Gt),
        Just(BinaryOp::Ge),
        Just(BinaryOp::In),
        Just(BinaryOp::Add),
        Just(BinaryOp::Sub),
        Just(BinaryOp::Mul),
        Just(BinaryOp::Div),
        Just(BinaryOp::Rem),
    ]
}

fn expr() -> impl Strategy<Value = Expr> {
    leaf().prop_recursive(5, 48, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Expr::List),
            inner
                .clone()
                .prop_map(|e| Expr::Unary(UnaryOp::Not, Box::new(e))),
            inner
                .clone()
                .prop_map(|e| Expr::Unary(UnaryOp::Neg, Box::new(e))),
            (binary_op(), inner.clone(), inner.clone()).prop_map(|(op, l, r)| Expr::Binary(
                op,
                Box::new(l),
                Box::new(r)
            )),
            (inner.clone(), inner.clone(), inner.clone()).prop_map(|(c, a, b)| Expr::Ternary(
                Box::new(c),
                Box::new(a),
                Box::new(b)
            )),
            (0..CALLS.len()).prop_flat_map({
                let inner = inner.clone();
                move |k| {
                    let (name, n) = CALLS[k];
                    proptest::collection::vec(inner.clone(), n..=n)
                        .prop_map(move |args| Expr::Call(name.to_owned(), args))
                }
            }),
        ]
    })
}

fn context() -> MapContext {
    MapContext::new(DateTime::from_micros(1_790_000_000_000_000).unwrap())
        .with("doc.a", Value::Int(3))
        .with("doc.b", Value::Text("x".into()))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn printing_then_parsing_gives_the_same_tree(e in expr()) {
        let text = e.to_string();
        let parsed = parse(&text);
        prop_assert!(parsed.is_ok(), "`{text}` did not parse: {:?}", parsed.err());
        let parsed = parsed.unwrap();
        prop_assert_eq!(&parsed, &e, "`{}`", text);
        prop_assert_eq!(parsed.to_string(), text);
    }

    #[test]
    fn parse_of_print_of_parse_is_parse(source in "[ -~]{0,40}") {
        if let Ok(first) = parse(&source) {
            let again = parse(&first.to_string());
            prop_assert_eq!(again, Ok(first));
        }
    }

    #[test]
    fn parsing_arbitrary_text_never_panics(source in "\\PC{0,60}") {
        let _ = parse(&source);
    }

    #[test]
    fn evaluation_never_panics(e in expr()) {
        let _ = e.eval(&context());
    }
}

/// `a op b` on decimals `(mantissa, scale)`, computed with plain `i128` arithmetic.
fn oracle(op: char, a: (i128, u32), b: (i128, u32)) -> (i128, u32) {
    let s = a.1.max(b.1);
    let align = |(m, sc): (i128, u32)| m * 10_i128.pow(s - sc);
    match op {
        '+' => (align(a) + align(b), s),
        '-' => (align(a) - align(b), s),
        _ => {
            // Exact product at scale a.1 + b.1, then half-even down to scale s.
            let exact = a.0 * b.0;
            let drop = a.1 + b.1 - s;
            let unit = 10_i128.pow(drop);
            let (q, r) = (exact.div_euclid(unit), exact.rem_euclid(unit));
            let twice = r * 2;
            let q = if twice > unit || (twice == unit && q % 2 != 0) {
                q + 1
            } else {
                q
            };
            (q, s)
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn decimal_results_match_the_scaled_integer_oracle(
        ma in -999_999_999_999_i64..=999_999_999_999,
        sa in 0_u32..=9,
        mb in -999_999_999_999_i64..=999_999_999_999,
        sb in 0_u32..=9,
        op in prop_oneof![Just('+'), Just('-'), Just('*')],
    ) {
        let a = Decimal::from_i128_with_scale(i128::from(ma), sa);
        let b = Decimal::from_i128_with_scale(i128::from(mb), sb);
        let ctx = MapContext::new(DateTime::from_micros(0).unwrap())
            .with("doc.a", Value::Decimal(a))
            .with("doc.b", Value::Decimal(b));
        let e = parse(&format!("doc.a {op} doc.b")).unwrap();
        let got = e.eval(&ctx);
        let (m, s) = oracle(op, (i128::from(ma), sa), (i128::from(mb), sb));
        let want = Decimal::from_i128_with_scale(m, s);
        match got {
            Ok(Value::Decimal(d)) => {
                prop_assert_eq!(d, want);
                prop_assert_eq!(d.scale(), s);
                prop_assert_eq!(d.mantissa(), m);
            }
            other => prop_assert!(false, "expected a decimal, got {:?}", other),
        }
    }
}
