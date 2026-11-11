//! The node handler never runs inside the store's transaction: while a slow handler is asleep,
//! no `bpm-tx` backend holds a transaction open. Asserted from `pg_stat_activity`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use spot_bpm_engine::{
    CaseId, Engine, HandlerRegistry, NoTimers, NodeCtx, NodeHandler, NodeKind, NodeOutcome,
};
use spot_bpm_pg::PgCaseStore;
use spot_dag_exec::RunStatus;
use spot_postgres::testing::TestDb;

/// How long the handler sleeps. A store transaction that spanned it would be visible as a
/// transaction at least this old.
const SLEEP: Duration = Duration::from_millis(1200);

/// The peak a single store transaction may reach, well above a commit and well below `SLEEP`.
const PEAK_BOUND_MS: u64 = 600;

/// A document handler that sleeps on its first call and then succeeds.
struct SlowHandler {
    slept: Arc<AtomicU64>,
}

impl NodeHandler for SlowHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Document
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        if self.slept.fetch_add(1, Ordering::SeqCst) == 0 {
            std::thread::sleep(SLEEP);
        }
        NodeOutcome::Done {
            output: None,
            branches: ctx.outgoing_conditions(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_handler_never_runs_inside_a_store_transaction() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = PgCaseStore::new(pool(&db, 4, "bpm-tx"));
    store.prepare().await.unwrap();

    let mut registry = HandlerRegistry::logic_default();
    registry.register(SlowHandler {
        slept: Arc::new(AtomicU64::new(0)),
    });
    let engine = Engine::new(store, NoTimers, registry);
    let process = process();
    let id = CaseId::new("case_tx").unwrap();
    engine
        .start(&process, id.clone(), "tx-key", None, now())
        .await
        .unwrap();

    // A poller on its own pool samples the age of every open `bpm-tx` transaction.
    let poller = pool(&db, 2, "tx-poll");
    let peak_ms = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let poll = {
        let peak_ms = peak_ms.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                let conn = poller.get().await.unwrap();
                let row = conn
                    .query_one(
                        "SELECT COALESCE(max(extract(milliseconds from now() - xact_start))::bigint, 0) \
                         FROM pg_stat_activity \
                         WHERE application_name = $1 AND xact_start IS NOT NULL \
                           AND pid <> pg_backend_pid()",
                        &[&"bpm-tx"],
                    )
                    .await
                    .unwrap();
                let ms: i64 = row.get(0);
                peak_ms.fetch_max(u64::try_from(ms).unwrap_or(0), Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };

    let started = Instant::now();
    let record = engine.run_until_idle(&id, now(), 20).await.unwrap();
    let elapsed = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    poll.await.unwrap();

    assert_eq!(record.run.status, RunStatus::Completed);
    assert!(
        elapsed >= SLEEP,
        "the handler slept ({elapsed:?}), so the check is meaningful"
    );
    let peak = peak_ms.load(Ordering::SeqCst);
    assert!(
        peak < PEAK_BOUND_MS,
        "a store transaction outlived the handler: peak {peak} ms (handler slept {SLEEP:?})"
    );
}
