//! Injection properties: values never reach the SQL text, and placeholders match parameters.

mod common;

use std::collections::BTreeSet;

use common::*;
use proptest::prelude::*;
use spot_storage_contract::{AggFunc, CmpOp, Expr, Nulls, OrderDir, Query, TextMode, Value};

/// Placeholder numbers used in `sql`.
fn placeholders(sql: &str) -> BTreeSet<usize> {
    let bytes = sql.as_bytes();
    let mut found = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let digits: String = sql[i + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(n) = digits.parse() {
                found.insert(n);
            }
        }
        i += 1;
    }
    found
}

/// A query touching every place a text value can go: comparisons, `IN`, every match mode with
/// and without case folding, `HAVING` and `COALESCE`.
fn query_with(values: &[String; 3]) -> Query {
    let mut q = party(&["name"]);
    let [a, b, c] = values;
    let mut conds = vec![
        f("p", "name").eq(text(a)),
        cmp(f("p", "name"), CmpOp::Lt, text(b)),
        Expr::In(
            Box::new(f("p", "note")),
            vec![Value::from(a.as_str()), Value::from(c.as_str())],
        ),
    ];
    for mode in [
        TextMode::Equals,
        TextMode::Prefix,
        TextMode::Contains,
        TextMode::Suffix,
    ] {
        conds.push(matches(f("p", "name"), mode, b, false));
        conds.push(matches(f("p", "note"), mode, c, true));
    }
    q.filter = Some(Expr::And(conds));
    q.select.push(sel(
        Expr::Coalesce(vec![f("p", "note"), text(a)]),
        "note_or",
    ));
    q.order_by = vec![order(f("p", "name"), OrderDir::Asc, Nulls::Last)];
    q.limit = Some(10);
    q
}

/// The same with aggregation, so `HAVING` and `GROUP BY` are covered.
fn grouped_with(values: &[String; 3]) -> Query {
    let mut q = party(&[]);
    q.select = vec![
        sel(f("p", "name"), "name"),
        sel(agg(AggFunc::Count, None, false), "n"),
    ];
    q.group_by = vec![f("p", "name")];
    q.having = Some(Expr::And(vec![
        cmp(f("p", "name"), CmpOp::Ne, text(&values[0])),
        matches(f("p", "name"), TextMode::Contains, &values[1], true),
    ]));
    q
}

/// Text values built from the characters that matter to SQL, led by a sentinel that SQL text
/// can never contain (all identifiers and keywords are ASCII).
fn hostile() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("'".to_owned()),
        Just("\"".to_owned()),
        Just(";".to_owned()),
        Just("--".to_owned()),
        Just("$1".to_owned()),
        Just("\\".to_owned()),
        Just("%".to_owned()),
        Just("_".to_owned()),
        Just("/*".to_owned()),
        Just("é ß 日本 😀".to_owned()),
        "[^\\x00]{0,6}",
    ];
    prop::collection::vec(piece, 0..6).prop_map(|v| format!("☃{}", v.concat()))
}

proptest! {
    #[test]
    fn text_values_never_reach_the_sql(vals in prop::array::uniform3(hostile())) {
        for q in [query_with(&vals), grouped_with(&vals)] {
            let c = ok(&q);
            for v in &vals {
                prop_assert!(!c.sql.contains(v.as_str()), "{v:?} leaked into {}", c.sql);
            }
            prop_assert!(c.sql.is_ascii(), "{}", c.sql);
            // Placeholders are exactly $1..=$n, one per parameter.
            let used = placeholders(&c.sql);
            let want: BTreeSet<usize> = (1..=c.params.len()).collect();
            prop_assert_eq!(used, want);
        }
    }

    #[test]
    fn sql_does_not_depend_on_the_values(
        a in prop::array::uniform3(hostile()),
        b in prop::array::uniform3(hostile()),
    ) {
        prop_assert_eq!(ok(&query_with(&a)).sql, ok(&query_with(&b)).sql);
        prop_assert_eq!(ok(&grouped_with(&a)).sql, ok(&grouped_with(&b)).sql);
    }

    #[test]
    fn numbers_never_reach_the_sql(i in any::<i64>(), limit in 0u32..u32::MAX, offset in 0u64..i64::MAX as u64) {
        let mut q = party(&["name"]);
        q.filter = Some(cmp(f("p", "visits"), CmpOp::Eq, int(i)));
        q.limit = Some(limit);
        q.offset = offset;
        let c = ok(&q);
        prop_assert_eq!(c.params.len(), 3);
        let digits: String = c.sql.chars().filter(char::is_ascii_digit).collect();
        // The only digits are placeholder numbers.
        prop_assert_eq!(digits, "123");
    }
}
