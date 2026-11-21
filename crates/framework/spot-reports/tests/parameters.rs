//! Typed parameters: an optional parameter drops its filter branch, a supplied one filters, and
//! an unknown or ill-typed argument is `report.bad_argument`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use serde_json::json;
use spot_reports::{NoTelemetry, ReportArgs, ReportRunner, RunOutput};
use support::{FakeExecutor, JOURNAL_FILTERED, MEMO_FILTER, compile, principals, world};

async fn run(
    world: &support::World,
    toml: &str,
    who: &str,
    filters: serde_json::Value,
) -> RunOutput {
    let report = compile(world, toml).expect("compiles");
    let principal = principals()
        .into_iter()
        .find(|(name, _)| *name == who)
        .expect("principal")
        .1;
    let args = ReportArgs::from_json(&filters).expect("args");
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
        .run(&report, &principal, &args, None)
        .await
        .expect("runs")
}

#[tokio::test]
async fn no_arguments_runs_without_filtering() {
    let world = world().await;
    let out = run(&world, JOURNAL_FILTERED, "alice", json!({})).await;
    assert_eq!(out.result.rows.len(), 1000);
    assert!(!out.result.truncated);
}

#[tokio::test]
async fn a_select_parameter_filters() {
    let world = world().await;
    let out = run(&world, JOURNAL_FILTERED, "alice", json!({"kind": "asset"})).await;
    let expected = world
        .journals
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 4 == 0)
        .count();
    assert_eq!(out.result.rows.len(), expected);
}

#[tokio::test]
async fn a_decimal_parameter_filters() {
    let world = world().await;
    let out = run(
        &world,
        JOURNAL_FILTERED,
        "alice",
        json!({"min_amount": "10.00"}),
    )
    .await;
    let threshold = rust_decimal::Decimal::new(1000, 2);
    let expected = world
        .journals
        .iter()
        .filter(|s| s.debit >= threshold)
        .count();
    assert!(expected > 0);
    assert_eq!(out.result.rows.len(), expected);
}

#[tokio::test]
async fn a_link_parameter_filters() {
    let world = world().await;
    let company = world.companies[0];
    let out = run(
        &world,
        JOURNAL_FILTERED,
        "alice",
        json!({"company": company.to_string()}),
    )
    .await;
    let expected = world
        .journals
        .iter()
        .enumerate()
        .filter(|(i, _)| (i % 4) % 3 == 0)
        .count();
    assert!(expected > 0);
    assert_eq!(out.result.rows.len(), expected);
}

#[tokio::test]
async fn an_unknown_argument_is_bad_argument_naming_it() {
    let world = world().await;
    let report = compile(&world, JOURNAL_FILTERED).expect("compiles");
    let args = ReportArgs::from_json(&json!({"nope": "x"})).expect("args");
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
    let err = runner
        .run(&report, &principals()[0].1, &args, None)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), "report.bad_argument");
    assert_eq!(err.path(), Some("nope"));
}

#[tokio::test]
async fn a_wrongly_typed_argument_is_bad_argument() {
    let world = world().await;
    let report = compile(&world, JOURNAL_FILTERED).expect("compiles");
    let args = ReportArgs::from_json(&json!({"kind": 3})).expect("args");
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
    let err = runner
        .run(&report, &principals()[0].1, &args, None)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), "report.bad_argument");
    assert_eq!(err.path(), Some("kind"));
}

#[tokio::test]
async fn a_missing_required_argument_is_bad_argument() {
    let world = world().await;
    let report = compile(&world, MEMO_FILTER).expect("compiles");
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
    let err = runner
        .run(&report, &principals()[0].1, &ReportArgs::empty(), None)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), "report.bad_argument");
    assert_eq!(err.path(), Some("memo"));
}
