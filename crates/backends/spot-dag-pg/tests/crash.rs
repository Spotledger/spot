//! The kill test of WP-DAG-02.T2: a process restart between node completions resumes with no
//! repeated and no lost node.
//!
//! The store's pool is dropped (every connection closes) and a fresh pool is opened, exactly as
//! a restarted process would. State survives in the database, and the resumed run completes
//! with every node done exactly once.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_dag_exec::{NodeStatus, RunId, RunStatus};
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_between_nodes_loses_and_repeats_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let dag = chain(5);
    let run_id = "restart_run";
    let key = "restart-key";

    // Phase one: create the run and finish the first two nodes, then drop the store and its
    // pool. Every connection is closed; nothing is in memory.
    {
        let store = store(&db, 4, "phase-1");
        store.prepare().await.unwrap();
        let mut run = create(&store, &dag, run_id, key).await;
        run = run_node(&store, &dag, &run, &nid("n0")).await;
        run = run_node(&store, &dag, &run, &nid("n1")).await;
        assert_eq!(run.node(&nid("n0")).unwrap().status, NodeStatus::Done);
        assert_eq!(run.node(&nid("n1")).unwrap().status, NodeStatus::Done);
        assert_eq!(run.node(&nid("n2")).unwrap().status, NodeStatus::Ready);
        assert_eq!(run.status, RunStatus::Running);
        // `store` and its pool are dropped here: a process restart.
    }

    // Phase two: a fresh pool, a fresh store over the same database.
    let store = store(&db, 4, "phase-2");
    let id = RunId::new(run_id).unwrap();
    let mut run = store.load_run(&id).await.unwrap().unwrap();
    assert_eq!(
        run.node(&nid("n0")).unwrap().attempts,
        1,
        "the finished node survived the restart exactly once"
    );
    assert_eq!(run.node(&nid("n1")).unwrap().attempts, 1);

    // Resume: nothing is repeated and nothing is lost.
    while let Some(node) = run.ready_nodes().first().cloned() {
        run = run_node(&store, &dag, &run, &node).await;
    }
    assert_eq!(run.status, RunStatus::Completed, "the run finishes");
    assert!(run.is_finished(), "nothing is left ready or running");
    for i in 0..5 {
        let state = run.node(&nid(&format!("n{i}"))).unwrap();
        assert_eq!(state.status, NodeStatus::Done, "n{i} completed");
        assert_eq!(state.attempts, 1, "n{i} ran exactly once");
        assert!(state.started_at.is_some() && state.ended_at.is_some());
    }

    // The business key still names the one run.
    let found = store.find_by_key(key).await.unwrap().unwrap();
    assert_eq!(found, run);
}

/// A completion already stored before the restart is a no-op when it is retried after it: the
/// idempotency key `(run, node, attempt)` survives the process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_completion_after_a_restart_is_a_no_op() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let dag = chain(3);
    let id = RunId::new("retry_run").unwrap();

    let (transition, before, stored) = {
        let store = store(&db, 4, "retry-1");
        store.prepare().await.unwrap();
        let run = create(&store, &dag, "retry_run", "retry-key").await;
        let begin = spot_dag_exec::claim(&run, &nid("n0"), now()).unwrap();
        let running = store.apply(&begin, run.version).await.unwrap();
        let done = spot_dag_exec::complete(
            &dag,
            &running,
            &nid("n0"),
            spot_dag_exec::NodeOutcome::Done {
                output: None,
                branches: Vec::new(),
            },
            now(),
        )
        .unwrap();
        let stored = store.apply(&done, running.version).await.unwrap();
        // The completion is retried against the version it was computed from, after a restart.
        (done, running.version, stored)
    };

    let store = store(&db, 4, "retry-2");
    let retried = store.apply(&transition, before).await.unwrap();
    assert_eq!(retried, stored, "the retried completion changes nothing");
    assert_eq!(retried.version, stored.version);
    assert_eq!(retried.node(&nid("n0")).unwrap().attempts, 1);
    assert_eq!(
        retried.node(&nid("n1")).unwrap().status,
        NodeStatus::Ready,
        "the successor is released exactly once"
    );
    let loaded = store.load_run(&id).await.unwrap().unwrap();
    assert_eq!(loaded, stored);
}
