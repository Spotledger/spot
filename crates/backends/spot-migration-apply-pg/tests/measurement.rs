//! The 100-branch measurement (WP-MIG-05's timing request, WP-MIG-07.T2 item 6).
//!
//! `#[ignore]`d, so the normal `cargo test` never pays for it: run it with
//!
//! ```text
//! SPOT_TEST_PG_URL=... cargo test -p spot-migration-apply-pg --test measurement \
//!     -- --ignored --nocapture
//! ```
//!
//! It builds a 100-branch catalogue (one table per branch), takes a snapshot, applies the plan
//! and restores the snapshot, and prints the three measurements as integers — microseconds
//! only, never a float. The step timings come from the runner's [`TimingSink`]; the snapshot
//! and restore timings come from the snapshot port's own capture.
//!
//! `SPOT_MEASURE_BRANCHES` scales the branch count (default 100).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_core::SystemClock;
use spot_migration_apply::{ApplyEngine, ApplyJournal, ApplyRequest, SnapshotPort};
use spot_migration_apply_pg::{PgApplyJournal, PgApplySchema, PgStepRunner, RecordingTimings};
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the 100-branch measurement; run with --ignored --nocapture"]
async fn the_100_branch_snapshot_apply_restore_measurement() {
    let Some(db) = db().await else { return };
    let branches: usize = std::env::var("SPOT_MEASURE_BRANCHES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100);
    let pool = pool(&db, 8);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let backend = backend(&db).await;
    let catalog = catalog(branches);
    let plan = schema_plan(branches);
    let timings = Arc::new(RecordingTimings::new());
    let snapshot = Arc::new(TestSnapshotPort::new(
        pool.clone(),
        site(),
        branch_tables(branches),
        Arc::new(SystemClock::new()),
        timings.clone(),
    ));
    let snapshot_id = snapshot.take("measure", 1_000).await.unwrap();

    let mut runner = PgStepRunner::new(
        backend.clone(),
        site(),
        catalog.clone(),
        engine_steps(&plan),
        Arc::new(SystemClock::new()),
    )
    .with_timings(timings.clone());
    runner.prepare().await.unwrap();
    let journal: Arc<dyn ApplyJournal> = Arc::new(PgApplyJournal::new(pool.clone(), site()));
    let gate = Arc::new(spot_migration_apply_pg::PgAdmissionGate::new(
        pool.clone(),
        site(),
        Arc::new(SystemClock::new()),
        std::time::Duration::from_millis(5),
    ));
    gate.refresh(1_000).await.unwrap();
    let engine = ApplyEngine::new(
        site(),
        journal,
        gate,
        Arc::new(spot_migration_apply::RecordingHold::new()),
        snapshot.clone(),
        Arc::new(runner),
        Arc::new(spot_migration_apply_pg::PgDriftVerifier::new(
            backend.clone(),
            catalog,
        )),
    );
    let ticket = ticket(&plan);
    let started = std::time::Instant::now();
    engine
        .apply(ApplyRequest::new(
            plan.clone(),
            Some(ticket),
            Some(snapshot_id.clone()),
            1_000,
        ))
        .await
        .expect("the measurement run applies");
    let whole = started.elapsed();

    let restored = snapshot.restore(&snapshot_id, 2_000).await.unwrap();
    assert_eq!(restored.snapshot, snapshot_id);

    let records = timings.records();
    print_timings("measurement", &records);
    let snapshot_micros: i64 = records
        .iter()
        .filter(|record| record.phase == spot_migration_apply_pg::TimingPhase::Snapshot)
        .map(|record| record.duration_micros)
        .sum();
    let apply_micros: i64 = records
        .iter()
        .filter(|record| record.phase == spot_migration_apply_pg::TimingPhase::Apply)
        .map(|record| record.duration_micros)
        .sum();
    let restore_micros: i64 = records
        .iter()
        .filter(|record| record.phase == spot_migration_apply_pg::TimingPhase::Restore)
        .map(|record| record.duration_micros)
        .sum();
    println!(
        "MEASUREMENT branches={branches} snapshot_micros={snapshot_micros} \
         apply_micros={apply_micros} restore_micros={restore_micros} \
         whole_millis={}",
        whole.as_millis()
    );
    assert!(apply_micros > 0 || branches == 0);
}
