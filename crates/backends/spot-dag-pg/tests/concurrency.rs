//! Two workers never take the same node: the claim is exactly once, and the ready-list skips a
//! run another session is holding (`FOR UPDATE SKIP LOCKED`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_dag_exec::{NodeStatus, claim};
use spot_postgres::testing::TestDb;

/// Two concurrent claims of the same ready node: one runs it, the other sees it already running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_never_take_the_same_node() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(store(&db, 8, "dag_race"));
    store.prepare().await.unwrap();
    let dag = chain(2);
    let run = create(&store, &dag, "race_run", "race-key").await;

    let begin = claim(&run, &nid("n0"), now()).unwrap();
    let expected = run.version;
    let left = begin.clone();
    let a = store.clone();
    let b = store.clone();
    let (first, second) =
        tokio::join!(async move { a.apply(&begin, expected).await }, async move {
            b.apply(&left, expected).await
        },);
    let first = first.unwrap();
    let second = second.unwrap();

    assert_eq!(first.version, expected + 1, "one claim advanced the run");
    assert_eq!(
        second.version,
        expected + 1,
        "the other returned the same run"
    );
    assert_eq!(first, second, "both callers see exactly one claim");
    assert_eq!(first.node(&nid("n0")).unwrap().status, NodeStatus::Running);
    assert_eq!(first.node(&nid("n0")).unwrap().attempts, 1);

    let stored = store.load_run(&run.id).await.unwrap().unwrap();
    assert_eq!(stored.node(&nid("n0")).unwrap().attempts, 1, "claimed once");
    assert_eq!(stored.version, expected + 1, "version advanced once");
}

/// The ready-list skips a run whose row another session holds, then offers it once released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_ready_skips_a_run_another_session_holds() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "dag_skip");
    store.prepare().await.unwrap();
    let dag = chain(2);
    let run = create(&store, &dag, "skip_run", "skip-key").await;

    // Another session holds the run row, as a worker mid-claim would.
    let mut client = db.raw_client().await;
    let tx = client.transaction().await.unwrap();
    tx.execute(
        "SELECT id FROM spot_dag_run WHERE id = $1 FOR UPDATE",
        &[&run.id.as_str()],
    )
    .await
    .unwrap();

    // The ready-list skips it instead of blocking on the lock.
    let ready = store.list_ready(10).await.unwrap();
    assert!(
        ready.iter().all(|item| item.run != run.id),
        "the locked run is skipped"
    );

    tx.rollback().await.unwrap();

    // Once released it is offered again, with its ready node.
    let ready = store.list_ready(10).await.unwrap();
    let mine = ready
        .iter()
        .find(|item| item.run == run.id)
        .expect("offered after release");
    assert_eq!(mine.ready, vec![nid("n0")]);
}
