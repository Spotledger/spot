//! Scale: a 100,000-row report, its stated memory bound, and an `#[ignore]` timing test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Instant;

use spot_reports::{NoTelemetry, ReportArgs, ReportRunner, RunOutput};
use support::{BIG_LIST, FakeExecutor, compile, principals, world_with};

/// The stated memory bound: the engine holds at most `limit + 1` rows at once, so a run's
/// result is bounded by the report's maximum rows, never by the table's size. This is the
/// per-row allowance (a title and a decimal string plus JSON overhead) the test checks.
const BYTES_PER_ROW: usize = 256;

async fn run(world: &support::World, who: &str, limit: Option<u64>) -> RunOutput {
    let report = compile(world, BIG_LIST).expect("compiles");
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

fn approx_bytes(out: &RunOutput) -> usize {
    out.result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| cell.as_str().map_or(16, str::len) + 24)
                .sum::<usize>()
        })
        .sum()
}

#[tokio::test]
async fn a_hundred_thousand_row_report_stays_within_its_row_bound() {
    let world = world_with(100_000).await;
    let out = run(&world, "alice", None).await;
    assert_eq!(out.result.rows.len(), 100_000);
    assert!(
        !out.result.truncated,
        "the report's maximum covers every row"
    );
    let bytes = approx_bytes(&out);
    assert!(
        bytes <= 100_000 * BYTES_PER_ROW,
        "a 100,000-row result must stay under {} bytes, was {bytes}",
        100_000 * BYTES_PER_ROW
    );
}

#[tokio::test]
#[ignore = "timing test for L9; run with --ignored"]
async fn timing_a_hundred_thousand_row_report() {
    let world = world_with(100_000).await;
    let started = Instant::now();
    let out = run(&world, "alice", None).await;
    let elapsed = started.elapsed();
    println!(
        "L9 report timing: 100000 rows in {} ms ({} rows returned)",
        elapsed.as_millis(),
        out.result.rows.len()
    );
    assert_eq!(out.result.rows.len(), 100_000);
}
