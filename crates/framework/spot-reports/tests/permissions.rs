//! Permission filtering and field protection: each principal sees exactly the rows the
//! decision core allows, document by document; a protected column is removed or masked; a
//! filter on a field the caller cannot read is refused.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use serde_json::json;
use spot_permissions::{MaskFormat, Principal};
use spot_reports::{NoTelemetry, ReportArgs, ReportRunner, RunOutput};
use support::{
    FakeExecutor, JOURNAL_BY_ACCOUNT, JOURNAL_IDS, MEMO_FILTER, MEMO_REPORT, carol, compile,
    principals, world,
};

fn runner(world: &support::World) -> ReportRunner {
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    ReportRunner::new(
        world.engine.clone(),
        world.guard.clone(),
        executor,
        Arc::new(NoTelemetry),
    )
}

async fn run(
    world: &support::World,
    toml: &str,
    principal: &Principal,
    args: ReportArgs,
) -> RunOutput {
    let report = compile(world, toml).expect("compiles");
    runner(world)
        .run(&report, principal, &args, None)
        .await
        .expect("runs")
}

fn ids(out: &RunOutput) -> Vec<String> {
    let mut ids: Vec<String> = out
        .result
        .rows
        .iter()
        .map(|r| r[0].as_str().expect("id").to_owned())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn every_principal_sees_exactly_the_rows_the_decision_core_allows() {
    let world = world().await;
    let mut all = principals();
    all.push(("carol", carol(&world.accounts)));
    for (name, principal) in &all {
        let out = run(&world, JOURNAL_IDS, principal, ReportArgs::empty()).await;
        let mut expected = world.allowed_journals(principal);
        expected.sort();
        assert_eq!(ids(&out), expected, "{name} sees exactly what it may read");
    }
}

#[tokio::test]
async fn a_reader_without_the_report_right_on_a_field_loses_the_column() {
    let world = world().await;
    let reader = principal("alice");
    let out = run(&world, MEMO_REPORT, &reader, ReportArgs::empty()).await;
    assert_eq!(out.result.columns.len(), 1);
    assert_eq!(out.result.columns[0].name, "title");
    assert_eq!(out.messages.len(), 1);
    assert_eq!(out.messages[0].code, "report.columns_removed");
    assert!(out.result.rows.iter().all(|r| r.len() == 1));
}

#[tokio::test]
async fn a_masked_reader_gets_the_masked_value() {
    let world = world().await;
    let masked = principal("erin");
    let out = run(&world, MEMO_REPORT, &masked, ReportArgs::empty()).await;
    assert_eq!(out.result.columns.len(), 2);
    let first = out.result.rows[0][1].as_str().expect("memo");
    let expected = spot_permissions::mask("memo-0000", &MaskFormat::LastChars(2));
    assert_eq!(first, expected);
    assert_ne!(first, "memo-0000");
}

#[tokio::test]
async fn an_analyst_with_the_report_right_sees_the_value() {
    let world = world().await;
    let analyst = principal("gina");
    let out = run(&world, MEMO_REPORT, &analyst, ReportArgs::empty()).await;
    assert_eq!(out.result.columns.len(), 2);
    assert_eq!(out.result.rows[0][1], json!("memo-0000"));
}

#[tokio::test]
async fn a_filter_on_a_hidden_field_is_refused() {
    let world = world().await;
    let reader = principal("alice");
    let report = compile(&world, MEMO_FILTER).expect("compiles");
    let args = ReportArgs::from_json(&json!({"memo": "memo-0000"})).expect("args");
    let err = runner(&world)
        .run(&report, &reader, &args, None)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), "permission.field_denied");
}

#[tokio::test]
async fn the_same_filter_is_allowed_for_a_reader_who_may_read_the_field() {
    let world = world().await;
    let analyst = principal("gina");
    let out = run(
        &world,
        MEMO_FILTER,
        &analyst,
        ReportArgs::from_json(&json!({"memo": "memo-0000"})).expect("args"),
    )
    .await;
    assert_eq!(out.result.rows.len(), 1);
    assert_eq!(out.result.rows[0][0], json!("journal-0000"));
}

#[tokio::test]
async fn a_principal_with_no_read_right_gets_an_empty_result_not_an_error() {
    let world = world().await;
    for name in ["dave", "frank"] {
        let principal = principal(name);
        let out = run(&world, JOURNAL_IDS, &principal, ReportArgs::empty()).await;
        assert!(out.result.rows.is_empty(), "{name} sees no rows");
        assert!(!out.result.truncated);
    }
}

fn principal(name: &str) -> Principal {
    principals()
        .into_iter()
        .find(|(n, _)| *n == name)
        .expect("principal")
        .1
}

#[tokio::test]
async fn an_unreadable_joined_row_yields_nulls_not_a_lost_base_row() {
    let world = world().await;
    let ivy = support::ivy(&world.companies);
    let out = run(&world, JOURNAL_BY_ACCOUNT, &ivy, ReportArgs::empty()).await;
    assert_eq!(out.result.rows.len(), 1000, "every journal row is kept");
    // Row i is `journal-i` (ordered by title); its account is `i % 4`, whose company is
    // `(i % 4) % 3`. Ivy may read only the accounts of company 0.
    for (i, row) in out.result.rows.iter().enumerate() {
        let account = i % 4;
        if account % 3 == 0 {
            assert_eq!(row[1], json!(format!("acct-{account}")), "row {i}");
        } else {
            assert_eq!(row[1], serde_json::Value::Null, "row {i}");
        }
    }
}
