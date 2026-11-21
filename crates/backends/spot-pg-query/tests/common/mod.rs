//! Shared helpers: the contract suite's catalog and short builders for queries.
#![allow(dead_code)]

use rust_decimal::Decimal;
use spot_pg_query::{CompiledQuery, DefaultSchemaMap, compile};
use spot_storage_contract::contract_tests::catalog;
use spot_storage_contract::{
    AggFunc, Catalog, CmpOp, Expr, Name, Nulls, Order, OrderDir, Query, QueryPath, Selection,
    StorageResult, TextMatch, TextMode, Value,
};

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

pub fn cat() -> Catalog {
    catalog().unwrap_or_else(|e| panic!("{e}"))
}

/// `alias.field`.
pub fn f(alias: &str, field: &str) -> Expr {
    Expr::field(&n(alias), &n(field))
}

pub fn text(s: &str) -> Expr {
    Expr::value(s)
}

pub fn int(i: i64) -> Expr {
    Expr::Value(Value::Int(i))
}

pub fn dec(units: i64, scale: u32) -> Expr {
    Expr::Value(Value::Decimal(Decimal::new(units, scale)))
}

pub fn sel(expr: Expr, alias: &str) -> Selection {
    Selection {
        expr,
        alias: n(alias),
    }
}

pub fn order(expr: Expr, dir: OrderDir, nulls: Nulls) -> Order {
    Order { expr, dir, nulls }
}

pub fn agg(func: AggFunc, arg: Option<Expr>, distinct: bool) -> Expr {
    Expr::Agg {
        func,
        arg: arg.map(Box::new),
        distinct,
    }
}

pub fn matches(e: Expr, mode: TextMode, pattern: &str, ci: bool) -> Expr {
    Expr::Text(
        Box::new(e),
        TextMatch {
            mode,
            pattern: pattern.to_owned(),
            case_insensitive: ci,
        },
    )
}

/// `SELECT p.<col> AS <col> FROM party p`.
pub fn party(cols: &[&str]) -> Query {
    let mut q = Query::from(&n("party"), &n("p"), QueryPath::Transactional);
    q.select = cols.iter().map(|c| sel(f("p", c), c)).collect();
    q
}

pub fn cmp(a: Expr, op: CmpOp, b: Expr) -> Expr {
    a.cmp(op, b)
}

pub fn run(q: &Query) -> StorageResult<CompiledQuery> {
    compile(q, &cat(), &DefaultSchemaMap)
}

pub fn ok(q: &Query) -> CompiledQuery {
    run(q).unwrap_or_else(|e| panic!("{e}"))
}
