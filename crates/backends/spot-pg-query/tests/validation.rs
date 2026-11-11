//! Validation parity with the contract suite (`query_is_validated`, aggregate lock rules),
//! the compiler's own type checks, and determinism.

mod common;

use common::*;
use spot_pg_query::{DefaultSchemaMap, Param, compile};
use spot_storage_contract::{
    AggFunc, ArithOp, CmpOp, ErrorKind, Expr, Nulls, OrderDir, Query, QueryPath, RowLock, TextMode,
    Value, Wait,
};

fn kind(q: &Query) -> Option<ErrorKind> {
    run(q).err().map(|e| e.kind)
}

fn count_star() -> Expr {
    agg(AggFunc::Count, None, false)
}

#[test]
fn validation_parity_with_the_contract_suite() {
    let base = || party(&["name"]);

    let mut q = base();
    q.filter = Some(f("p", "colour").eq(text("red")));
    assert_eq!(kind(&q), Some(ErrorKind::UnknownField), "unknown field");

    let mut q = base();
    q.filter = Some(f("p", "note").eq(Expr::Value(Value::Null)));
    assert_eq!(
        kind(&q),
        Some(ErrorKind::InvalidQuery),
        "comparison with null"
    );

    let mut q = base();
    q.filter = Some(count_star().eq(int(1)));
    assert_eq!(
        kind(&q),
        Some(ErrorKind::InvalidQuery),
        "aggregate in filter"
    );

    let mut q = base();
    q.select.push(sel(count_star(), "n"));
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "ungrouped field");

    let mut q = base();
    q.filter = Some(f("p", "credit_limit").eq(text("lots")));
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "type mismatch");

    let mut q = Query::from(&n("nowhere"), &n("p"), QueryPath::Report);
    q.select = vec![sel(f("p", "name"), "name")];
    assert_eq!(kind(&q), Some(ErrorKind::UnknownEntity), "unknown entity");

    let mut q = base();
    q.select = vec![];
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "empty select");

    let mut q = base();
    q.select.push(sel(f("p", "name"), "name"));
    assert_eq!(
        kind(&q),
        Some(ErrorKind::InvalidQuery),
        "duplicate column alias"
    );

    let mut q = base();
    q.filter = Some(Expr::In(Box::new(f("p", "name")), vec![Value::Null]));
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "null in IN list");

    let mut q = base();
    q.filter = Some(matches(f("p", "visits"), TextMode::Prefix, "x", false));
    assert_eq!(
        kind(&q),
        Some(ErrorKind::InvalidQuery),
        "text match on a number"
    );

    let mut q = party(&[]);
    q.select = vec![sel(count_star(), "n")];
    q.lock = RowLock::Update(Wait::Block);
    assert_eq!(
        kind(&q),
        Some(ErrorKind::InvalidQuery),
        "lock on an aggregate"
    );
}

#[test]
fn compiler_type_checks_give_invalid_query() {
    let base = || party(&["name"]);

    // Fields of different types.
    let mut q = base();
    q.filter = Some(f("p", "name").eq(f("p", "visits")));
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "text = int");

    // Arithmetic and SUM need numbers.
    let mut q = base();
    q.select = vec![sel(
        Expr::Arith(Box::new(f("p", "name")), ArithOp::Add, Box::new(int(1))),
        "x",
    )];
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "text + int");
    let mut q = party(&[]);
    q.select = vec![sel(agg(AggFunc::Sum, Some(f("p", "name")), false), "x")];
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "sum of text");

    // JSON has no order.
    let mut q = party(&[]);
    q.select = vec![sel(agg(AggFunc::Max, Some(f("p", "profile")), false), "x")];
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "max of json");

    // A condition must be boolean.
    let mut q = base();
    q.filter = Some(f("p", "name"));
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "text as filter");
    let mut q = base();
    q.filter = Some(Expr::And(vec![f("p", "visits")]));
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "int in AND");

    // COALESCE.
    let mut q = base();
    q.select = vec![sel(Expr::Coalesce(vec![]), "x")];
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "empty coalesce");
    let mut q = base();
    q.select = vec![sel(Expr::Coalesce(vec![f("p", "name"), int(1)]), "x")];
    assert_eq!(
        kind(&q),
        Some(ErrorKind::InvalidQuery),
        "coalesce text and int"
    );

    // IN with a value of the wrong type.
    let mut q = base();
    q.filter = Some(Expr::In(Box::new(f("p", "name")), vec![Value::Int(1)]));
    assert_eq!(kind(&q), Some(ErrorKind::InvalidQuery), "text IN ints");

    // Offsets PostgreSQL cannot represent.
    let mut q = base();
    q.offset = u64::MAX;
    assert_eq!(
        kind(&q),
        Some(ErrorKind::InvalidQuery),
        "offset beyond int8"
    );

    // NUL cannot be stored in PostgreSQL text.
    let mut q = base();
    q.filter = Some(f("p", "name").eq(text("a\0b")));
    assert_eq!(kind(&q), Some(ErrorKind::InvalidValue), "NUL in text");
}

#[test]
fn compiling_twice_gives_identical_output() {
    let mut q = party(&["name", "visits"]);
    q.filter = Some(Expr::And(vec![
        matches(f("p", "name"), TextMode::Contains, "a", true),
        cmp(f("p", "visits"), CmpOp::Gt, int(3)),
    ]));
    q.order_by = vec![order(f("p", "name"), OrderDir::Desc, Nulls::First)];
    q.limit = Some(5);
    assert_eq!(ok(&q), ok(&q));
    assert_eq!(ok(&q).sql, ok(&q.clone()).sql);
}

#[test]
fn parameters_are_typed() {
    let mut q = party(&["name"]);
    q.filter = Some(Expr::And(vec![
        f("p", "name").eq(text("x")),
        cmp(f("p", "visits"), CmpOp::Eq, int(1)),
        cmp(f("p", "credit_limit"), CmpOp::Eq, dec(15, 1)),
        cmp(f("p", "active"), CmpOp::Eq, Expr::Value(Value::Bool(true))),
    ]));
    let c = ok(&q);
    let types: Vec<_> = c.params.iter().map(|p| p.pg_type().sql_name()).collect();
    assert_eq!(types, ["text", "int8", "numeric", "bool", "int8"]);
    assert!(matches!(c.params[2], Param::Numeric(_)));
}

#[test]
fn a_custom_schema_map_is_quoted_and_checked() {
    use spot_pg_query::SchemaMap;
    use spot_storage_contract::Name;

    struct Evil;
    impl SchemaMap for Evil {
        fn table(&self, entity: &Name) -> String {
            format!("t\"{entity}")
        }
        fn column(&self, _: &Name, field: &Name) -> String {
            field.as_str().to_owned()
        }
    }
    struct Empty;
    impl SchemaMap for Empty {
        fn table(&self, _: &Name) -> String {
            String::new()
        }
        fn column(&self, _: &Name, field: &Name) -> String {
            field.as_str().to_owned()
        }
    }
    let q = party(&["name"]);
    let c = compile(&q, &cat(), &Evil).unwrap_or_else(|e| panic!("{e}"));
    assert!(c.sql.contains("\"t\"\"party\""), "{}", c.sql);
    let err = compile(&q, &cat(), &Empty).err().map(|e| e.kind);
    assert_eq!(err, Some(ErrorKind::InvalidName));
    // The default map is the identity.
    assert!(ok(&q).sql.contains("FROM \"party\" AS \"p\""));
    let _ = DefaultSchemaMap;
}
