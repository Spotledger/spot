//! The store's error mapping and edge cases, on a real database: the `dag_exec.*` codes the
//! `ExecStore` trait promises, plus `prepare` idempotence.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_dag_exec::{
    ExecError, NodeOutcome, NodeStatus, RunId, RunStatus, cancel, claim, complete, start,
};
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "dag_prepare");
    store.prepare().await.unwrap();
    store.prepare().await.unwrap();
    // A run can be stored and read back.
    let dag = chain(2);
    let run = create(&store, &dag, "prepare_run", "prepare-key").await;
    assert_eq!(store.load_run(&run.id).await.unwrap().unwrap(), run);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_ids_and_keys_are_reported() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "dag_dupes");
    store.prepare().await.unwrap();
    let dag = chain(2);
    let run = create(&store, &dag, "dupe_run", "dupe-key").await;

    // The same id.
    let again = start(
        &dag,
        &published(&dag),
        RunId::new("dupe_run").unwrap(),
        "other-key",
        now(),
    )
    .unwrap();
    assert!(matches!(
        store.create_run(again).await,
        Err(ExecError::RunExists { .. })
    ));

    // A fresh id, the same business key.
    let same_key = start(
        &dag,
        &published(&dag),
        RunId::new("dupe_run2").unwrap(),
        "dupe-key",
        now(),
    )
    .unwrap();
    assert!(matches!(
        store.create_run(same_key).await,
        Err(ExecError::KeyConflict { .. })
    ));

    // The original run is untouched.
    assert_eq!(store.load_run(&run.id).await.unwrap().unwrap(), run);
    assert!(
        store
            .load_run(&RunId::new("dupe_run2").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn absent_runs_and_keys_are_none() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "dag_absent");
    store.prepare().await.unwrap();
    assert!(
        store
            .load_run(&RunId::new("never_run").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.find_by_key("never-key").await.unwrap().is_none());
    // Applying to an absent run is `dag_exec.run_not_found`.
    let dag = chain(2);
    let plan = start(
        &dag,
        &published(&dag),
        RunId::new("ghost_run").unwrap(),
        "ghost-key",
        now(),
    )
    .unwrap();
    let ghost = plan.run;
    let begin = claim(&ghost, &nid("n0"), now()).unwrap();
    assert!(matches!(
        store.apply(&begin, ghost.version).await,
        Err(ExecError::RunNotFound { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_version_conflicts_and_a_bad_step_is_refused() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "dag_conflict");
    store.prepare().await.unwrap();
    let dag = chain(2);
    let run = create(&store, &dag, "conflict_run", "conflict-key").await;

    // A stale version writes nothing.
    let begin = claim(&run, &nid("n0"), now()).unwrap();
    assert!(matches!(
        store.apply(&begin, run.version + 9).await,
        Err(ExecError::VersionConflict { .. })
    ));
    assert_eq!(store.load_run(&run.id).await.unwrap().unwrap(), run);

    // A claim of a node that is not ready is refused (n1 is still pending).
    let not_ready = spot_dag_exec::Transition {
        run: run.id.clone(),
        step: spot_dag_exec::Step::Claim {
            node: nid("n1"),
            attempt: 1,
            at: spot_dag_exec::Instant::from_offset(now()),
        },
    };
    assert!(matches!(
        store.apply(&not_ready, run.version).await,
        Err(ExecError::InvalidState { .. })
    ));
    assert_eq!(store.load_run(&run.id).await.unwrap().unwrap(), run);

    // A transition naming a node the run does not track is refused.
    let begin = claim(&run, &nid("n0"), now()).unwrap();
    let running = store.apply(&begin, run.version).await.unwrap();
    let ghost = spot_dag_exec::Transition {
        run: run.id.clone(),
        step: spot_dag_exec::Step::Claim {
            node: nid("ghost"),
            attempt: 1,
            at: spot_dag_exec::Instant::from_offset(now()),
        },
    };
    assert!(matches!(
        store.apply(&ghost, running.version).await,
        Err(ExecError::InvalidNode { .. })
    ));

    let done = complete(
        &dag,
        &running,
        &nid("n0"),
        NodeOutcome::Done {
            output: None,
            branches: Vec::new(),
        },
        now(),
    )
    .unwrap();
    let after = store.apply(&done, running.version).await.unwrap();
    assert_eq!(after.node(&nid("n0")).unwrap().status, NodeStatus::Done);
    assert_eq!(after.node(&nid("n1")).unwrap().status, NodeStatus::Ready);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_stops_the_run_and_repeats_as_a_no_op() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "dag_cancel");
    store.prepare().await.unwrap();
    let dag = chain(3);
    let run = create(&store, &dag, "cancel_run", "cancel-key").await;

    let cancelled = cancel(&run, now()).unwrap();
    let once = store.apply(&cancelled, run.version).await.unwrap();
    assert_eq!(once.status, RunStatus::Cancelled);
    assert!(once.ended_at.is_some());

    let twice = store.apply(&cancelled, run.version).await.unwrap();
    assert_eq!(twice, once, "a repeated cancel is a no-op");
    assert_eq!(twice.version, once.version);

    // A cancelled run is not offered as ready work.
    let ready = store.list_ready(10).await.unwrap();
    assert!(ready.iter().all(|item| item.run != run.id));
}
