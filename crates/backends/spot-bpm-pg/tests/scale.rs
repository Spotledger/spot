//! 10,000 Cases complete inside a generous time budget: WP-BPM-02's scale acceptance on
//! PostgreSQL. The number the run reports is recorded in the PR.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use spot_bpm_engine::{CaseId, EffectLedger, Engine, NoTimers};
use spot_bpm_pg::PgCaseStore;
use spot_dag_exec::RunStatus;
use spot_postgres::testing::TestDb;

const CASES: usize = 10_000;
/// The whole run. A quiet machine finishes in a couple of minutes; the bound is deliberately
/// generous for a loaded shared runner and still catches a store that serialises every Case.
const TIME_BOUND: Duration = Duration::from_secs(1200);

/// The one-node process the scale run uses must compile before the run starts.
#[test]
fn the_two_node_process_compiles() {
    spot_bpm_engine::ProcessDefinition::compile(&two_node()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ten_thousand_cases_complete_within_the_budget() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = PgCaseStore::new(pool(&db, 32, "bpm-scale"));
    store.prepare().await.unwrap();
    let ledger = Arc::new(EffectLedger::new());
    let engine = Arc::new(Engine::new(store, NoTimers, handlers(&ledger)));
    let process = two_node();
    let definition = engine.publish(&process).unwrap();

    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..CASES {
        let engine = engine.clone();
        let definition = definition.clone();
        tasks.spawn(async move {
            let id = CaseId::new(&format!("scale_{index}")).unwrap();
            engine
                .start_definition(
                    &definition,
                    id.clone(),
                    format!("scale-key-{index}"),
                    None,
                    now(),
                )
                .await
                .unwrap();
            let record = engine.run_until_idle(&id, now(), 20).await.unwrap();
            assert_eq!(record.run.status, RunStatus::Completed, "case {index}");
        });
    }
    while tasks.join_next().await.is_some() {}
    let elapsed = started.elapsed();
    println!("{CASES} Cases completed in {elapsed:?} (bound {TIME_BOUND:?})");

    assert!(
        elapsed < TIME_BOUND,
        "took {elapsed:?}, bound {TIME_BOUND:?}"
    );
    assert_eq!(ledger.distinct(), CASES, "one side effect per Case");
    assert_eq!(ledger.total(), CASES as u64);
    assert!(ledger.duplicates().is_empty());
}
