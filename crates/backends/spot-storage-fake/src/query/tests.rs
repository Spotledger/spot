//! Unit tests of the evaluator: truth tables, coalesce, arithmetic, aggregates and ordering.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use spot_storage_contract::query::{AggFunc, ArithOp, CmpOp, Expr, Nulls, Order, OrderDir};
use spot_storage_contract::schema::system;
use spot_storage_contract::{Collation, ErrorKind, Fields, Name, RecordId, Value, Version};

use super::compare_keys;
use super::eval::{Binding, Env, Evaluator, Scope, collator_for};
use crate::store::{Record, RecordKind};

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn evaluator<'a>(aliases: Vec<&'a Name>) -> Evaluator<'a> {
    let collation = Collation::new("en").unwrap_or_else(|e| panic!("{e}"));
    Evaluator::new(
        aliases,
        collator_for(&collation).unwrap_or_else(|e| panic!("{e}")),
    )
}

fn lit(v: impl Into<Value>) -> Expr {
    Expr::value(v)
}

fn null() -> Expr {
    Expr::Value(Value::Null)
}

fn dec(units: i64, scale: u32) -> Value {
    Value::Decimal(Decimal::new(units, scale))
}

fn eval(e: &Expr) -> Value {
    evaluator(vec![])
        .eval(e, Scope::Row(&Vec::new()))
        .unwrap_or_else(|e| panic!("{e}"))
}

fn record(v: Value) -> Record {
    let mut fields = Fields::new();
    fields.insert(n("v"), v);
    Record {
        fields,
        kind: RecordKind::Document {
            version: Version::FIRST,
            children: BTreeMap::new(),
        },
    }
}

/// Aggregates `func` over the `v` field of one row per value.
fn agg(func: AggFunc, distinct: bool, values: Vec<Value>) -> Value {
    let alias = n("t");
    let records: Vec<(RecordId, Record)> = values
        .into_iter()
        .map(|v| (RecordId::new_v7(), record(v)))
        .collect();
    let envs: Vec<Env<'_>> = records
        .iter()
        .map(|(id, record)| vec![Some(Binding { id: *id, record })])
        .collect();
    let refs: Vec<&Env<'_>> = envs.iter().collect();
    let e = Expr::Agg {
        func,
        arg: Some(Box::new(Expr::field(&alias, &n("v")))),
        distinct,
    };
    evaluator(vec![&alias])
        .eval(&e, Scope::Group(&refs))
        .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn and_follows_the_sql_truth_table() {
    let (t, f, u) = (|| lit(true), || lit(false), null);
    let cases = [
        (vec![t(), t()], Value::Bool(true)),
        (vec![t(), f()], Value::Bool(false)),
        (vec![t(), u()], Value::Null),
        (vec![f(), u()], Value::Bool(false)),
        (vec![u(), f()], Value::Bool(false)),
        (vec![u(), u()], Value::Null),
        (vec![], Value::Bool(true)),
    ];
    for (parts, want) in cases {
        assert_eq!(eval(&Expr::And(parts.clone())), want, "{parts:?}");
    }
}

#[test]
fn or_follows_the_sql_truth_table() {
    let (t, f, u) = (|| lit(true), || lit(false), null);
    let cases = [
        (vec![f(), f()], Value::Bool(false)),
        (vec![t(), f()], Value::Bool(true)),
        (vec![f(), u()], Value::Null),
        (vec![t(), u()], Value::Bool(true)),
        (vec![u(), t()], Value::Bool(true)),
        (vec![u(), u()], Value::Null),
        (vec![], Value::Bool(false)),
    ];
    for (parts, want) in cases {
        assert_eq!(eval(&Expr::Or(parts.clone())), want, "{parts:?}");
    }
}

#[test]
fn not_of_unknown_is_unknown() {
    assert_eq!(eval(&Expr::Not(Box::new(lit(true)))), Value::Bool(false));
    assert_eq!(eval(&Expr::Not(Box::new(lit(false)))), Value::Bool(true));
    assert_eq!(eval(&Expr::Not(Box::new(null()))), Value::Null);
}

#[test]
fn a_comparison_involving_null_is_unknown() {
    for op in [CmpOp::Eq, CmpOp::Ne, CmpOp::Lt, CmpOp::Ge] {
        assert_eq!(eval(&null().cmp(op, lit(1))), Value::Null);
        assert_eq!(eval(&lit(1).cmp(op, null())), Value::Null);
    }
    assert_eq!(
        eval(&Expr::IsNull(Box::new(null()))),
        Value::Bool(true),
        "IsNull is the way to test for null"
    );
    assert_eq!(
        eval(&Expr::In(Box::new(null()), vec![Value::Int(1)])),
        Value::Null
    );
    assert_eq!(
        eval(&Expr::In(
            Box::new(lit(2)),
            vec![Value::Int(1), Value::Int(2)]
        )),
        Value::Bool(true)
    );
}

#[test]
fn int_and_decimal_compare_as_numbers() {
    assert_eq!(eval(&lit(2).eq(lit(dec(200, 2)))), Value::Bool(true));
    assert_eq!(
        eval(&lit(dec(250, 2)).cmp(CmpOp::Gt, lit(2))),
        Value::Bool(true)
    );
    assert_eq!(
        eval(&lit(dec(10, 1)).eq(lit(dec(100, 2)))),
        Value::Bool(true),
        "1.0 equals 1.00"
    );
}

#[test]
fn text_equality_is_exact_and_ordering_follows_the_collation() {
    assert_eq!(eval(&lit("ali").eq(lit("Ali"))), Value::Bool(false));
    assert_eq!(
        eval(&lit("Zebra").cmp(CmpOp::Lt, lit("b"))),
        Value::Bool(false)
    );
    assert_eq!(
        eval(&lit("apple").cmp(CmpOp::Lt, lit("Apple"))),
        Value::Bool(true)
    );
    assert_eq!(
        eval(&lit("eclair").cmp(CmpOp::Lt, lit("éclair"))),
        Value::Bool(true)
    );
}

#[test]
fn coalesce_takes_the_first_non_null() {
    assert_eq!(
        eval(&Expr::Coalesce(vec![null(), lit(2), lit(3)])),
        Value::Int(2)
    );
    assert_eq!(eval(&Expr::Coalesce(vec![null(), null()])), Value::Null);
    assert_eq!(eval(&Expr::Coalesce(vec![])), Value::Null);
}

#[test]
fn arithmetic_is_exact_and_null_propagates() {
    let add = |a: Expr, b: Expr| Expr::Arith(Box::new(a), ArithOp::Add, Box::new(b));
    assert_eq!(eval(&add(lit(1), lit(2))), Value::Int(3));
    assert_eq!(eval(&add(lit(1), lit(dec(25, 1)))), dec(35, 1));
    assert_eq!(eval(&add(lit(1), null())), Value::Null);
    let mul = Expr::Arith(
        Box::new(lit(dec(15, 1))),
        ArithOp::Mul,
        Box::new(lit(dec(15, 1))),
    );
    assert_eq!(eval(&mul), dec(225, 2));
}

#[test]
fn arithmetic_overflow_is_invalid_value() {
    let ev = evaluator(vec![]);
    let env = Vec::new();
    for (a, op, b) in [
        (Value::Int(i64::MAX), ArithOp::Add, Value::Int(1)),
        (Value::Int(i64::MIN), ArithOp::Sub, Value::Int(1)),
        (Value::Int(i64::MAX), ArithOp::Mul, Value::Int(2)),
        (Value::Decimal(Decimal::MAX), ArithOp::Add, dec(1, 0)),
        (Value::Decimal(Decimal::MAX), ArithOp::Mul, Value::Int(2)),
    ] {
        let e = Expr::Arith(Box::new(Expr::Value(a)), op, Box::new(Expr::Value(b)));
        let kind = ev.eval(&e, Scope::Row(&env)).err().map(|err| err.kind);
        assert_eq!(kind, Some(ErrorKind::InvalidValue), "{e:?}");
    }
}

#[test]
fn count_star_counts_rows_and_count_of_a_field_counts_non_null_values() {
    let alias = n("t");
    let ids: Vec<(RecordId, Record)> = (0..3)
        .map(|i| {
            (
                RecordId::new_v7(),
                record(if i == 1 { Value::Null } else { Value::Int(7) }),
            )
        })
        .collect();
    let envs: Vec<Env<'_>> = ids
        .iter()
        .map(|(id, record)| vec![Some(Binding { id: *id, record })])
        .collect();
    let refs: Vec<&Env<'_>> = envs.iter().collect();
    let ev = evaluator(vec![&alias]);
    let star = Expr::Agg {
        func: AggFunc::Count,
        arg: None,
        distinct: false,
    };
    assert_eq!(
        ev.eval(&star, Scope::Group(&refs))
            .unwrap_or_else(|e| panic!("{e}")),
        Value::Int(3)
    );
    assert_eq!(
        agg(
            AggFunc::Count,
            false,
            vec![Value::Int(7), Value::Null, Value::Int(7)]
        ),
        Value::Int(2)
    );
}

#[test]
fn distinct_counts_distinct_non_null_values() {
    let values = vec![
        Value::Int(1),
        Value::Int(1),
        Value::Null,
        Value::Int(2),
        dec(100, 2),
    ];
    // 1 and 1.00 are the same number.
    assert_eq!(agg(AggFunc::Count, true, values), Value::Int(2));
    let texts = vec!["a".into(), "A".into(), "a".into(), Value::Null];
    assert_eq!(
        agg(AggFunc::Count, true, texts),
        Value::Int(2),
        "text is exact"
    );
}

#[test]
fn sum_min_max_ignore_nulls_and_give_null_over_nothing() {
    let vs = || vec![Value::Int(3), Value::Null, Value::Int(5)];
    assert_eq!(agg(AggFunc::Sum, false, vs()), Value::Int(8));
    assert_eq!(agg(AggFunc::Min, false, vs()), Value::Int(3));
    assert_eq!(agg(AggFunc::Max, false, vs()), Value::Int(5));
    for func in [AggFunc::Sum, AggFunc::Min, AggFunc::Max] {
        assert_eq!(agg(func, false, vec![]), Value::Null);
        assert_eq!(agg(func, false, vec![Value::Null]), Value::Null);
    }
    assert_eq!(agg(AggFunc::Count, false, vec![]), Value::Int(0));
    assert_eq!(
        agg(AggFunc::Sum, false, vec![Value::Int(1), dec(25, 1)]),
        dec(35, 1)
    );
    assert_eq!(
        agg(
            AggFunc::Sum,
            true,
            vec![Value::Int(2), Value::Int(2), Value::Int(3)]
        ),
        Value::Int(5)
    );
}

#[test]
fn text_min_max_use_the_collation() {
    let names: Vec<Value> = ["Zebra", "apple", "Apple"]
        .iter()
        .map(|s| (*s).into())
        .collect();
    assert_eq!(
        agg(AggFunc::Min, false, names.clone()),
        Value::Text("apple".into())
    );
    assert_eq!(agg(AggFunc::Max, false, names), Value::Text("Zebra".into()));
}

fn order(dir: OrderDir, nulls: Nulls) -> Vec<Order> {
    vec![Order {
        expr: lit(0),
        dir,
        nulls,
    }]
}

fn sorted(dir: OrderDir, nulls: Nulls) -> Vec<Value> {
    let ev = evaluator(vec![]);
    let order = order(dir, nulls);
    let mut vs = vec![
        Value::Int(2),
        Value::Null,
        Value::Int(1),
        Value::Int(3),
        Value::Null,
    ];
    vs.sort_by(|a, b| {
        compare_keys(
            &ev,
            &order,
            std::slice::from_ref(a),
            std::slice::from_ref(b),
        )
    });
    vs
}

#[test]
fn nulls_are_placed_as_asked_in_both_directions() {
    use Value::{Int, Null};
    assert_eq!(
        sorted(OrderDir::Asc, Nulls::First),
        [Null, Null, Int(1), Int(2), Int(3)]
    );
    assert_eq!(
        sorted(OrderDir::Asc, Nulls::Last),
        [Int(1), Int(2), Int(3), Null, Null]
    );
    assert_eq!(
        sorted(OrderDir::Desc, Nulls::First),
        [Null, Null, Int(3), Int(2), Int(1)]
    );
    assert_eq!(
        sorted(OrderDir::Desc, Nulls::Last),
        [Int(3), Int(2), Int(1), Null, Null]
    );
}

#[test]
fn system_columns_read_from_a_record() {
    let alias = n("t");
    let r = record(Value::Int(1));
    let id = RecordId::new_v7();
    let env: Env<'_> = vec![Some(Binding { id, record: &r })];
    let e = Expr::field(&alias, &n(system::ID));
    let got = evaluator(vec![&alias])
        .eval(&e, Scope::Row(&env))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got, Value::Id(id));
    // A missing right row of a left join reads as null.
    let gone: Env<'_> = vec![None];
    let got = evaluator(vec![&alias])
        .eval(&Expr::field(&alias, &n("v")), Scope::Row(&gone))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got, Value::Null);
}
