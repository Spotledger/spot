//! Plain, joined, grouped and paged runs against the fake backend.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use spot_reports::{NoTelemetry, ReportArgs, ReportRunner, RunOutput};
use support::{
    FakeExecutor, JOURNAL_BY_ACCOUNT, JOURNAL_LIST, TRIAL_BALANCE, compile, principals, world,
};

async fn run(
    world: &support::World,
    toml: &str,
    who: &str,
    args: ReportArgs,
    limit: Option<u64>,
) -> RunOutput {
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
        .run(&report, &principal, &args, limit)
        .await
        .expect("runs")
}

#[tokio::test]
async fn a_plain_list_returns_the_seeded_rows() {
    let world = world().await;
    let out = run(&world, JOURNAL_LIST, "alice", ReportArgs::empty(), Some(3)).await;
    assert_eq!(out.result.columns.len(), 3);
    assert_eq!(out.result.columns[0].name, "title");
    assert_eq!(out.result.rows.len(), 3);
    assert!(out.result.truncated, "1000 rows cut to 3");
    assert_eq!(out.result.rows[0][0], serde_json::json!("journal-0000"));
    assert_eq!(out.result.rows[1][0], serde_json::json!("journal-0001"));
}

#[tokio::test]
async fn the_default_limit_truncates_a_thousand_rows() {
    let world = world().await;
    let out = run(&world, JOURNAL_LIST, "alice", ReportArgs::empty(), None).await;
    assert_eq!(out.result.rows.len(), 100, "the default limit is 100");
    assert!(out.result.truncated);
}

#[tokio::test]
async fn a_joined_report_reads_the_linked_doctype() {
    let world = world().await;
    let out = run(
        &world,
        JOURNAL_BY_ACCOUNT,
        "alice",
        ReportArgs::empty(),
        Some(5),
    )
    .await;
    assert_eq!(out.result.columns.len(), 2);
    assert_eq!(out.result.columns[0].name, "title");
    assert_eq!(out.result.columns[1].name, "acct_title");
    assert_eq!(out.result.rows[0][0], serde_json::json!("journal-0000"));
    assert_eq!(out.result.rows[0][1], serde_json::json!("acct-0"));
    assert_eq!(out.result.rows[1][1], serde_json::json!("acct-1"));
}

#[tokio::test]
async fn group_by_sums_equal_a_hand_computed_total_over_a_thousand_rows() {
    let world = world().await;
    let out = run(&world, TRIAL_BALANCE, "alice", ReportArgs::empty(), None).await;
    assert!(!out.result.truncated);

    let mut expected: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
    for (i, seed) in world.journals.iter().enumerate() {
        let title = format!("acct-{}", i % 4);
        let entry = expected
            .entry(title)
            .or_insert((Decimal::ZERO, Decimal::ZERO));
        entry.0 += seed.debit;
        entry.1 += seed.credit;
    }
    assert_eq!(out.result.rows.len(), expected.len());
    for row in &out.result.rows {
        let title = row[0].as_str().expect("title").to_owned();
        let (debit, credit) = expected.get(&title).expect("known account");
        assert_eq!(
            row[1],
            serde_json::json!(debit.to_string()),
            "{title} debit"
        );
        assert_eq!(
            row[2],
            serde_json::json!(credit.to_string()),
            "{title} credit"
        );
    }
}

#[tokio::test]
async fn the_same_input_gives_the_same_bytes() {
    let world = world().await;
    let a = run(&world, JOURNAL_LIST, "alice", ReportArgs::empty(), Some(50)).await;
    let b = run(&world, JOURNAL_LIST, "alice", ReportArgs::empty(), Some(50)).await;
    // `ReportResult` is not `Serialize`; its `Debug` form is deterministic and covers every
    // column and cell.
    assert_eq!(format!("{:?}", a.result), format!("{:?}", b.result));
}
