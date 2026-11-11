//! The engine end-to-end against the PostgreSQL store: a Case runs to completion, a restart
//! between steps resumes exactly once, and a chaos run over 500 Cases crashes and restarts at
//! random points with no duplicated and no lost side effect.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use spot_bpm_engine::{
    CaseId, EffectLedger, Engine, EngineError, EngineResult, Faults, NoFaults, NoTimers,
    ProcessDefinition, ScriptedFaults,
};
use spot_bpm_pg::PgCaseStore;
use spot_dag_exec::RunStatus;
use spot_postgres::testing::TestDb;

/// The engine under test, over a fresh store and the given fault injector.
fn build_engine(
    store: PgCaseStore,
    ledger: &Arc<EffectLedger>,
    faults: Arc<dyn Faults>,
) -> Engine<PgCaseStore, NoTimers> {
    Engine::new(store, NoTimers, handlers(ledger)).with_faults(faults)
}

/// A fault injector that crashes at each effect boundary with probability `1 / one_in`, drawn
/// from a deterministic xorshift64 seeded per Case (no randomness the test cannot reproduce).
struct RandomFaults {
    state: Mutex<u64>,
    one_in: u64,
}

impl RandomFaults {
    fn new(seed: u64, one_in: u64) -> Self {
        Self {
            state: Mutex::new(seed | 1),
            one_in,
        }
    }
}

impl Faults for RandomFaults {
    fn check(&self, point: &spot_bpm_engine::EffectPoint) -> EngineResult<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        if x.is_multiple_of(self.one_in) {
            Err(EngineError::Crashed {
                point: point.label(),
            })
        } else {
            Ok(())
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linear_case_runs_to_completion_with_one_effect() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = PgCaseStore::new(pool(&db, 4, "engine-linear"));
    store.prepare().await.unwrap();
    let ledger = Arc::new(EffectLedger::new());
    let engine = build_engine(store, &ledger, Arc::new(NoFaults));
    let process = process();
    let id = CaseId::new("case_linear").unwrap();
    engine
        .start(&process, id.clone(), "linear-key", None, now())
        .await
        .unwrap();
    let record = engine.run_until_idle(&id, now(), 20).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
    assert_eq!(ledger.performed("case_linear:work:1"), 1);
    assert!(ledger.duplicates().is_empty());
    assert_eq!(ledger.total(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_after_the_outcome_is_stored_replays_it_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = PgCaseStore::new(pool(&db, 4, "engine-crash"));
    store.prepare().await.unwrap();
    let ledger = Arc::new(EffectLedger::new());
    let process = process();
    let id = CaseId::new("case_crash").unwrap();

    // Phase one: crash right after the work node's outcome is stored, before its transition.
    {
        let faults = Arc::new(ScriptedFaults::after(7));
        let engine = build_engine(
            PgCaseStore::new(pool(&db, 4, "engine-crash-1")),
            &ledger,
            faults,
        );
        engine
            .start(&process, id.clone(), "crash-key", None, now())
            .await
            .unwrap();
        let mut crashed = false;
        for _ in 0..20 {
            match engine.step(&id, now()).await {
                Err(EngineError::Crashed { .. }) => {
                    crashed = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected error: {error}"),
            }
        }
        assert!(crashed, "the scripted fault must fire");
        // The engine (and its pool) is dropped here: a worker restart.
    }

    // Phase two: a fresh pool and engine resume from the durable outcome.
    let engine = build_engine(
        PgCaseStore::new(pool(&db, 4, "engine-crash-2")),
        &ledger,
        Arc::new(NoFaults),
    );
    engine.publish(&process).unwrap();
    let record = engine.run_until_idle(&id, now(), 20).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
    assert_eq!(
        ledger.performed("case_crash:work:1"),
        1,
        "the stored outcome was replayed, not re-executed"
    );
    assert!(ledger.duplicates().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chaos_run_over_five_hundred_cases_is_exactly_once() {
    const CASES: usize = 500;
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Prepare the schema once; every restarted store opens over the same database.
    PgCaseStore::new(pool(&db, 2, "chaos-prepare"))
        .prepare()
        .await
        .unwrap();

    let process = process();
    let definition: Arc<ProcessDefinition> =
        Arc::new(ProcessDefinition::compile(&process).unwrap());
    let ledger = Arc::new(EffectLedger::new());
    let mut total_restarts = 0_usize;
    // The worker's connection pool. A crash closes it (dropping the connections) and opens a
    // fresh one, exactly as a restarted process would.
    let mut current = pool(&db, 2, "chaos");

    for index in 0..CASES {
        let id = CaseId::new(&format!("chaos_{index}")).unwrap();
        let key = format!("chaos-key-{index}");
        let seed = (index as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(1);
        let faults = Arc::new(RandomFaults::new(seed, 32));
        let mut engine = build_engine(PgCaseStore::new(current.clone()), &ledger, faults.clone());
        engine.processes().publish((*definition).clone());
        engine
            .start_definition(&definition, id.clone(), key, None, now())
            .await
            .unwrap();

        let mut restarts = 0_usize;
        loop {
            match engine.step(&id, now()).await {
                Ok(report) => {
                    if report.finished {
                        break;
                    }
                }
                Err(EngineError::Crashed { .. }) => {
                    restarts += 1;
                    assert!(restarts < 500, "case {id} crashed too often");
                    // Kill the worker: drop the engine, close its pool, and start a fresh one.
                    drop(engine);
                    current.close();
                    current = pool(&db, 2, "chaos");
                    engine =
                        build_engine(PgCaseStore::new(current.clone()), &ledger, faults.clone());
                    engine.processes().publish((*definition).clone());
                }
                Err(error) => panic!("case {index}: {error}"),
            }
        }
        total_restarts += restarts;
    }

    // Every Case completed, exactly one side effect each, none duplicated and none lost.
    assert_eq!(ledger.distinct(), CASES, "one distinct effect per Case");
    assert_eq!(ledger.total(), CASES as u64, "one effect per Case");
    assert!(ledger.duplicates().is_empty(), "no effect ran twice");
    for index in 0..CASES {
        let id = CaseId::new(&format!("chaos_{index}")).unwrap();
        assert_eq!(
            ledger.performed(&format!("{id}:work:1")),
            1,
            "case {index} performed its effect exactly once"
        );
    }
    println!("{CASES} Cases survived {total_restarts} worker restarts");
    assert!(
        total_restarts > 0,
        "the chaos run must have crashed at least once"
    );
}
