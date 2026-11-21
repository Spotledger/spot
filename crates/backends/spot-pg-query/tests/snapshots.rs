//! Snapshot corpus: one case per rule of the crate docs, each named after its rule. A snapshot
//! holds the SQL, the parameter list and the decode types.

mod common;

use common::*;
use rust_decimal::Decimal;
use spot_pg_query::{CompiledQuery, DefaultSchemaMap, compile};
use spot_storage_contract::{
    AggFunc, CmpOp, Collation, Expr, Join, JoinKind, Nulls, OrderDir, Query, QueryPath, RowLock,
    Source, TextMode, Value, Wait,
};

fn render(c: &CompiledQuery) -> String {
    let mut out = format!("{}\n", c.sql);
    for (i, p) in c.params.iter().enumerate() {
        out.push_str(&format!("${} = {p:?}\n", i + 1));
    }
    out.push_str(&format!("columns = {:?}\n", c.columns));
    out.push_str(&format!("decode = {:?}\n", c.decode));
    out.push_str(&format!("path = {:?}\n", c.path));
    out
}

macro_rules! snap {
    ($name:literal, $q:expr) => {{
        insta::assert_snapshot!($name, render(&ok(&$q)));
    }};
}

#[test]
fn values_are_parameters_never_text() {
    let mut q = party(&["name", "credit_limit"]);
    q.filter = Some(Expr::And(vec![
        f("p", "name").eq(text("O'Brien; DROP TABLE party; --")),
        cmp(f("p", "credit_limit"), CmpOp::Gt, dec(1000, 2)),
        cmp(f("p", "visits"), CmpOp::Ge, int(7)),
        cmp(f("p", "active"), CmpOp::Eq, Expr::Value(Value::Bool(true))),
    ]));
    q.limit = Some(25);
    q.offset = 50;
    snap!("values_are_parameters_never_text", q);
}

#[test]
fn three_valued_not_stays_unknown() {
    let mut q = party(&["name"]);
    q.filter = Some(Expr::Not(Box::new(f("p", "name").eq(text("x")))));
    snap!("three_valued_not_stays_unknown", q);
    let mut q = party(&["name"]);
    q.filter = Some(Expr::Or(vec![
        Expr::Not(Box::new(Expr::IsNull(Box::new(f("p", "note"))))),
        Expr::Not(Box::new(Expr::And(vec![
            f("p", "visits").eq(int(1)),
            f("p", "active").eq(Expr::Value(Value::Bool(false))),
        ]))),
    ]));
    snap!("three_valued_nested_not_and_or", q);
}

#[test]
fn empty_junctions_and_empty_in() {
    let mut q = party(&["name"]);
    q.filter = Some(Expr::Or(vec![
        Expr::And(vec![]),
        Expr::Or(vec![]),
        Expr::In(Box::new(f("p", "name")), vec![]),
    ]));
    snap!("empty_junctions_and_empty_in", q);
}

#[test]
fn numbers_int_meets_decimal() {
    let mut q = party(&["name"]);
    q.filter = Some(Expr::And(vec![
        cmp(f("p", "visits"), CmpOp::Lt, dec(25, 1)),
        cmp(f("p", "credit_limit"), CmpOp::Ge, int(100)),
        cmp(f("p", "visits"), CmpOp::Le, f("p", "credit_limit")),
        cmp(f("p", "credit_limit"), CmpOp::Ne, f("p", "visits")),
    ]));
    snap!("numbers_int_meets_decimal", q);
}

#[test]
fn text_equality_in_and_ne_are_exact() {
    let mut q = party(&["name"]);
    q.filter = Some(Expr::And(vec![
        f("p", "name").eq(text("Ali")),
        cmp(f("p", "name"), CmpOp::Ne, text("ali")),
        cmp(text("Ali"), CmpOp::Eq, f("p", "name")),
        Expr::In(
            Box::new(f("p", "name")),
            vec![Value::from("Ali"), Value::from("ali")],
        ),
    ]));
    snap!("text_equality_in_and_ne_are_exact", q);
}

#[test]
fn in_list_mixes_int_and_decimal() {
    let mut q = party(&["name"]);
    q.filter = Some(Expr::Or(vec![
        Expr::In(
            Box::new(f("p", "visits")),
            vec![Value::Int(1), Value::Decimal(Decimal::new(25, 1))],
        ),
        Expr::In(
            Box::new(f("p", "credit_limit")),
            vec![Value::Int(1), Value::Decimal(Decimal::new(25, 1))],
        ),
        Expr::In(
            Box::new(f("p", "parent_party")),
            vec![Value::Id(spot_storage_contract::RecordId::new_v7())],
        ),
    ]));
    let c = ok(&q);
    insta::assert_snapshot!("in_list_mixes_int_and_decimal", {
        // The id is random; keep only the shape.
        let mut s = c.sql.clone();
        s.push_str(&format!("\n{} params", c.params.len()));
        s
    });
}

#[test]
fn text_range_uses_tenant_collation() {
    let mut q = party(&["name"]);
    q.filter = Some(Expr::And(vec![
        cmp(f("p", "name"), CmpOp::Lt, text("M")),
        cmp(f("p", "name"), CmpOp::Le, text("M")),
        cmp(f("p", "name"), CmpOp::Gt, text("A")),
        cmp(f("p", "name"), CmpOp::Ge, f("p", "note")),
    ]));
    snap!("text_range_uses_tenant_collation", q);
}

#[test]
fn order_by_text_adds_code_point_key_and_id_tail() {
    let mut q = party(&["name", "visits"]);
    q.order_by = vec![
        order(f("p", "name"), OrderDir::Desc, Nulls::First),
        order(f("p", "visits"), OrderDir::Asc, Nulls::Last),
        order(f("p", "note"), OrderDir::Asc, Nulls::First),
    ];
    snap!("order_by_text_adds_code_point_key_and_id_tail", q);
}

#[test]
fn text_matching_is_literal_without_like() {
    for (mode, name) in [
        (TextMode::Equals, "equals"),
        (TextMode::Prefix, "prefix"),
        (TextMode::Contains, "contains"),
        (TextMode::Suffix, "suffix"),
    ] {
        let mut q = party(&["name"]);
        q.filter = Some(matches(f("p", "name"), mode, "50%_\\off", false));
        let c = ok(&q);
        assert!(!c.sql.contains("LIKE"), "{}", c.sql);
        insta::assert_snapshot!(format!("text_matching_literal_{name}"), render(&c));
        let mut q = party(&["name"]);
        q.filter = Some(matches(f("p", "name"), mode, "Ali", true));
        insta::assert_snapshot!(
            format!("text_matching_case_insensitive_{name}"),
            render(&ok(&q))
        );
    }
}

#[test]
fn arithmetic_on_int_and_decimal() {
    let mut q = party(&["name"]);
    q.select = vec![
        sel(
            Expr::Arith(
                Box::new(f("p", "visits")),
                spot_storage_contract::ArithOp::Add,
                Box::new(int(1)),
            ),
            "int_sum",
        ),
        sel(
            Expr::Arith(
                Box::new(f("p", "credit_limit")),
                spot_storage_contract::ArithOp::Mul,
                Box::new(f("p", "visits")),
            ),
            "dec_product",
        ),
        sel(
            Expr::Arith(
                Box::new(f("p", "credit_limit")),
                spot_storage_contract::ArithOp::Sub,
                Box::new(dec(5, 1)),
            ),
            "dec_diff",
        ),
    ];
    snap!("arithmetic_on_int_and_decimal", q);
}

#[test]
fn aggregates_over_no_group() {
    let mut q = party(&[]);
    q.select = vec![
        sel(agg(AggFunc::Count, None, false), "n"),
        sel(agg(AggFunc::Count, Some(f("p", "note")), false), "n_note"),
        sel(agg(AggFunc::Count, Some(f("p", "name")), true), "n_names"),
        sel(
            agg(AggFunc::Count, Some(f("p", "visits")), true),
            "n_visits",
        ),
        sel(
            agg(AggFunc::Sum, Some(f("p", "visits")), false),
            "visits_sum",
        ),
        sel(
            agg(AggFunc::Sum, Some(f("p", "credit_limit")), false),
            "limit_sum",
        ),
        sel(
            agg(AggFunc::Sum, Some(f("p", "credit_limit")), true),
            "limit_sum_distinct",
        ),
        sel(agg(AggFunc::Min, Some(f("p", "name")), false), "first_name"),
        sel(agg(AggFunc::Max, Some(f("p", "name")), false), "last_name"),
        sel(
            agg(AggFunc::Min, Some(f("p", "since")), false),
            "first_since",
        ),
        sel(
            agg(AggFunc::Max, Some(f("p", "seen_at")), false),
            "last_seen",
        ),
        sel(
            agg(AggFunc::Min, Some(f("p", "opens_at")), false),
            "first_open",
        ),
        sel(
            agg(AggFunc::Min, Some(f("p", "active")), false),
            "all_active",
        ),
        sel(
            agg(AggFunc::Max, Some(f("p", "active")), false),
            "any_active",
        ),
        sel(agg(AggFunc::Min, Some(f("p", "id")), false), "min_id"),
        sel(agg(AggFunc::Max, Some(f("p", "logo")), false), "max_logo"),
    ];
    snap!("aggregates_over_no_group", q);
}

#[test]
fn group_by_having_and_text_keys() {
    let mut q = party(&[]);
    q.select = vec![
        sel(f("p", "name"), "name"),
        sel(f("p", "active"), "active"),
        sel(
            agg(AggFunc::Sum, Some(f("p", "credit_limit")), false),
            "total",
        ),
    ];
    q.filter = Some(cmp(f("p", "visits"), CmpOp::Gt, int(0)));
    q.group_by = vec![f("p", "name"), f("p", "active")];
    q.having = Some(Expr::And(vec![
        cmp(
            agg(AggFunc::Sum, Some(f("p", "credit_limit")), false),
            CmpOp::Gt,
            dec(0, 0),
        ),
        cmp(f("p", "name"), CmpOp::Ne, text("zz")),
        cmp(f("p", "name"), CmpOp::Lt, text("zz")),
    ]));
    q.order_by = vec![
        order(f("p", "name"), OrderDir::Asc, Nulls::Last),
        order(
            agg(AggFunc::Sum, Some(f("p", "credit_limit")), false),
            OrderDir::Desc,
            Nulls::Last,
        ),
    ];
    snap!("group_by_having_and_text_keys", q);
}

fn invoice_join(kind: JoinKind) -> Query {
    let mut q = Query::from(&n("invoice"), &n("i"), QueryPath::Report);
    q.joins = vec![Join {
        kind,
        source: Source {
            entity: n("party"),
            alias: n("p"),
        },
        on: f("i", "party").eq(f("p", "id")),
    }];
    q.select = vec![
        sel(f("i", "number"), "number"),
        sel(f("p", "name"), "party_name"),
    ];
    q.filter = Some(Expr::Or(vec![
        Expr::IsNull(Box::new(f("p", "id"))),
        f("p", "name").eq(text("x")),
    ]));
    q.order_by = vec![order(f("p", "name"), OrderDir::Asc, Nulls::Last)];
    q
}

#[test]
fn joins_inner_and_left() {
    snap!("joins_inner", invoice_join(JoinKind::Inner));
    snap!("joins_left", invoice_join(JoinKind::Left));
}

#[test]
fn child_entity_system_columns() {
    let mut q = Query::from(&n("invoice_line"), &n("l"), QueryPath::Transactional);
    q.select = vec![
        sel(f("l", "id"), "id"),
        sel(f("l", "parent"), "parent"),
        sel(f("l", "parent_field"), "parent_field"),
        sel(f("l", "idx"), "idx"),
        sel(f("l", "amount"), "amount"),
    ];
    q.filter = Some(f("l", "parent_field").eq(text("lines")));
    snap!("child_entity_system_columns", q);
    let mut q = party(&["name"]);
    q.select.push(sel(f("p", "version"), "version"));
    snap!("document_version_column", q);
}

#[test]
fn paging_limit_and_offset_are_parameters() {
    let mut q = party(&["name"]);
    q.limit = Some(10);
    snap!("paging_limit_only", q);
    let mut q = party(&["name"]);
    q.offset = 30;
    snap!("paging_offset_only", q);
}

#[test]
fn row_locks_lock_only_the_from_alias() {
    for (lock, name) in [
        (RowLock::Update(Wait::Block), "update_block"),
        (RowLock::Update(Wait::NoWait), "update_nowait"),
        (RowLock::Update(Wait::SkipLocked), "update_skip_locked"),
        (RowLock::Share(Wait::Block), "share_block"),
        (RowLock::Share(Wait::NoWait), "share_nowait"),
        (RowLock::Share(Wait::SkipLocked), "share_skip_locked"),
    ] {
        let mut q = invoice_join(JoinKind::Inner);
        q.lock = lock;
        q.limit = Some(1);
        insta::assert_snapshot!(format!("row_lock_{name}"), render(&ok(&q)));
    }
}

#[test]
fn coalesce_and_null_literals_are_typed_parameters() {
    let mut q = party(&["name"]);
    q.select = vec![
        sel(
            Expr::Coalesce(vec![f("p", "visits"), Expr::Value(Value::Null), int(0)]),
            "visits",
        ),
        sel(
            Expr::Coalesce(vec![
                Expr::Value(Value::Null),
                f("p", "credit_limit"),
                int(0),
            ]),
            "limit",
        ),
        sel(Expr::Value(Value::Null), "nothing"),
        sel(Expr::IsNull(Box::new(f("p", "note"))), "no_note"),
    ];
    snap!("coalesce_and_null_literals_are_typed_parameters", q);
}

#[test]
fn collation_name_follows_the_locale() {
    let mut ur = Query::from(&n("party"), &n("p"), QueryPath::Transactional);
    ur.select = vec![sel(f("p", "name"), "name")];
    ur.order_by = vec![order(f("p", "name"), OrderDir::Asc, Nulls::Last)];
    let base = cat();
    let entities: Vec<_> = base.entities().cloned().collect();
    let catalog = spot_storage_contract::Catalog::new(
        base.version,
        Collation::new("ur-PK").unwrap_or_else(|e| panic!("{e}")),
        entities,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let c = compile(&ur, &catalog, &DefaultSchemaMap).unwrap_or_else(|e| panic!("{e}"));
    insta::assert_snapshot!("collation_name_follows_the_locale", render(&c));
}

#[test]
fn query_path_passes_through() {
    let mut q = party(&["name"]);
    q.path = QueryPath::Report;
    assert_eq!(ok(&q).path, QueryPath::Report);
    q.path = QueryPath::Transactional;
    assert_eq!(ok(&q).path, QueryPath::Transactional);
}
