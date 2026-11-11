//! Computed columns and value encoding: parity with `spot-expr`, exact decimal strings, and the
//! `avg` decimal pair.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use serde_json::json;
use spot_expr::{Expr as LangExpr, MapContext, Value as LangValue};
use spot_reports::{NoTelemetry, ReportArgs, ReportRunner, RunOutput};
use support::{AVG_REPORT, COMPUTED_REPORT, FakeExecutor, compile, principals, world};

async fn run(world: &support::World, toml: &str, who: &str, limit: Option<u64>) -> RunOutput {
    let report = compile(world, toml).expect("compiles");
    let principal = principals()
        .into_iter()
        .find(|(name, _)| *name == who)
        .expect("principal")
        .1;
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    let runner = ReportRunner::new(
        world.engine.clone(),
        world.guard.clone(),
        executor,
        Arc::new(NoTelemetry),
    );
    runner
        .run(&report, &principal, &ReportArgs::empty(), limit)
        .await
        .expect("runs")
}

#[tokio::test]
async fn a_computed_column_matches_spot_expr_on_the_same_values() {
    let world = world().await;
    let out = run(&world, COMPUTED_REPORT, "alice", Some(50)).await;
    assert_eq!(out.result.columns.len(), 3);
    assert_eq!(out.result.columns[2].name, "net");

    let expr = spot_expr::parse("doc.debit - doc.credit").expect("parses");
    let now = spot_expr::DateTime::from_micros(0).expect("clock");
    for row in &out.result.rows {
        let debit = decimal(row[0].as_str().expect("debit"));
        let credit = decimal(row[1].as_str().expect("credit"));
        let ctx = MapContext::new(now)
            .with("doc.debit", LangValue::Decimal(debit))
            .with("doc.credit", LangValue::Decimal(credit));
        let expected = eval_decimal(&expr, &ctx);
        assert_eq!(row[2], json!(expected.to_string()));
    }
}

#[tokio::test]
async fn decimal_columns_keep_their_scale() {
    let world = world().await;
    let out = run(&world, COMPUTED_REPORT, "alice", Some(3)).await;
    // Every seeded decimal has scale 2, so every encoded string has two decimals.
    for row in &out.result.rows {
        for cell in [&row[0], &row[1], &row[2]] {
            let s = cell.as_str().expect("decimal string");
            assert_eq!(s.split('.').nth(1).map(str::len), Some(2), "{s}");
        }
    }
}

#[tokio::test]
async fn an_avg_column_is_a_decimal_pair_of_sum_and_count() {
    let world = world().await;
    let out = run(&world, AVG_REPORT, "alice", None).await;
    let mut sums: BTreeMap<String, (Decimal, i64)> = BTreeMap::new();
    for (i, seed) in world.journals.iter().enumerate() {
        let title = format!("acct-{}", i % 4);
        let entry = sums.entry(title).or_insert((Decimal::ZERO, 0));
        entry.0 += seed.debit;
        entry.1 += 1;
    }
    assert_eq!(out.result.rows.len(), sums.len());
    for row in &out.result.rows {
        let title = row[0].as_str().expect("title").to_owned();
        let pair = row[1].as_array().expect("pair");
        let (sum, count) = sums.get(&title).expect("known account");
        assert_eq!(pair[0], json!(sum.to_string()));
        assert_eq!(pair[1], json!(count));
    }
}

fn decimal(s: &str) -> Decimal {
    s.parse().expect("decimal")
}

fn eval_decimal(expr: &LangExpr, ctx: &MapContext) -> Decimal {
    let value = expr.eval(ctx).expect("evaluates");
    match spot_expr::mapping::to_storage(&value).expect("maps") {
        spot_storage_contract::Value::Decimal(d) => d,
        other => panic!("not a decimal: {other:?}"),
    }
}
