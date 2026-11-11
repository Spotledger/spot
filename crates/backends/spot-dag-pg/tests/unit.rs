//! The `Unit` port and `apply_in` on a real database: a document written through the document
//! engine's PostgreSQL backend and a node transition commit or roll back together.
//!
//! Each test needs `SPOT_TEST_PG_URL` and skips cleanly without it, as the other `*-pg` crates
//! do; the CI Postgres service sets it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use spot_app_api::error::{StorageOp, map_storage_error};
use spot_app_api::{AppError, ErrorClass};
use spot_dag::{CompiledDag, NodeId};
use spot_dag_exec::unit::WorkFuture;
use spot_dag_exec::{ExecError, ExecStore, NodeStatus, Transition, Unit, complete};
use spot_dag_pg::PgExecStore;
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, DocumentData, DocumentStore, Fields, Isolation, RecordId, RowLock, StorageBackend,
    Transaction, TxOptions, Value,
};

/// A document of the tests' `probe` entity (one non-null text field).
fn probe_doc() -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("label"), Value::Text("unit".to_owned()));
    DocumentData {
        entity: n("probe"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

/// A caller's `work` that inserts `doc` on the transaction it is given.
fn insert_work(
    doc: DocumentData,
) -> impl for<'t> FnMut(&'t mut PgTransaction) -> WorkFuture<'t> + Send {
    move |tx| {
        let doc = doc.clone();
        Box::pin(async move {
            tx.insert(&doc)
                .await
                .map_err(|error| map_storage_error(&error, StorageOp::Write))?;
            Ok(())
        })
    }
}

/// A caller's `work` that inserts `doc` and then fails: the failure after a real write.
fn fail_after_insert(
    doc: DocumentData,
) -> impl for<'t> FnMut(&'t mut PgTransaction) -> WorkFuture<'t> + Send {
    move |tx| {
        let doc = doc.clone();
        Box::pin(async move {
            tx.insert(&doc)
                .await
                .map_err(|error| map_storage_error(&error, StorageOp::Write))?;
            Err(AppError::new(
                ErrorClass::Internal,
                "probe.injected",
                "the document work failed after the insert",
            ))
        })
    }
}

/// A caller's `work` that writes nothing, for a retried transition.
fn no_work() -> impl for<'t> FnMut(&'t mut PgTransaction) -> WorkFuture<'t> + Send {
    |_tx: &mut PgTransaction| -> WorkFuture<'_> { Box::pin(async { Ok(()) }) }
}

/// Whether the document `id` is visible on a fresh transaction.
async fn document_exists(backend: &PgBackend, catalog: &Arc<Catalog>, id: RecordId) -> bool {
    let options = TxOptions::new(n("dag_unit_check"), Isolation::ReadCommitted);
    let mut tx = backend
        .begin(Arc::clone(catalog), options)
        .await
        .expect("check transaction");
    let found = tx
        .load(&n("probe"), id, RowLock::None)
        .await
        .expect("load")
        .is_some();
    tx.commit().await.expect("check commit");
    found
}

/// A store with its DAG tables and the `probe` document table prepared.
async fn prepared(db: &TestDb) -> (PgExecStore, PgBackend, Arc<Catalog>) {
    let store = store(db, 8, "dag_unit");
    store.prepare().await.unwrap();
    let backend = backend(db);
    let catalog = catalog();
    backend.prepare(&catalog).await.unwrap();
    (store, backend, catalog)
}

/// Claims `node` of `run`, leaving it `Running` at attempt 1.
async fn claim_node(
    store: &PgExecStore,
    run: &spot_dag_exec::Run,
    node: &NodeId,
) -> spot_dag_exec::Run {
    let begin = spot_dag_exec::claim(run, node, now()).unwrap();
    store.apply(&begin, run.version).await.unwrap()
}

/// The completion of `node` computed from `run`.
fn completion(dag: &CompiledDag, run: &spot_dag_exec::Run, node: &NodeId) -> Transition {
    complete(
        dag,
        run,
        node,
        spot_dag_exec::NodeOutcome::Done {
            output: None,
            branches: Vec::new(),
        },
        now(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_document_and_a_completion_commit_together() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (store, backend, catalog) = prepared(&db).await;
    let dag = chain(2);
    let run = create(&store, &dag, "unit_commit", "unit-commit-key").await;
    let running = claim_node(&store, &run, &nid("n0")).await;
    let done = completion(&dag, &running, &nid("n0"));

    let doc = probe_doc();
    let id = doc.id;
    let mut work = insert_work(doc);
    let after = store
        .commit_with(&done, running.version, &mut work)
        .await
        .expect("the unit commits");

    assert_eq!(after.node(&nid("n0")).unwrap().status, NodeStatus::Done);
    assert_eq!(after.node(&nid("n1")).unwrap().status, NodeStatus::Ready);
    assert_eq!(after.version, running.version + 1);
    assert!(
        document_exists(&backend, &catalog, id).await,
        "the document committed with the run"
    );
    assert_eq!(
        store.load_run(&run.id).await.unwrap().unwrap(),
        after,
        "the run and its nodes are stored"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_work_after_the_document_insert_leaves_neither() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (store, backend, catalog) = prepared(&db).await;
    let dag = chain(2);
    let run = create(&store, &dag, "unit_work_fail", "unit-work-fail-key").await;
    let running = claim_node(&store, &run, &nid("n0")).await;
    let done = completion(&dag, &running, &nid("n0"));

    let doc = probe_doc();
    let id = doc.id;
    let mut work = fail_after_insert(doc);
    let result = store.commit_with(&done, running.version, &mut work).await;
    match result {
        Err(ExecError::WorkFailed(_)) => {}
        other => panic!("the failing work must surface, got {other:?}"),
    }

    assert!(
        !document_exists(&backend, &catalog, id).await,
        "the document rolled back with the run"
    );
    let stored = store.load_run(&run.id).await.unwrap().unwrap();
    assert_eq!(stored.node(&nid("n0")).unwrap().status, NodeStatus::Running);
    assert_eq!(stored.version, running.version, "the run did not advance");
    assert_eq!(stored.node(&nid("n1")).unwrap().status, NodeStatus::Pending);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_transition_after_the_document_insert_leaves_neither() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (store, backend, catalog) = prepared(&db).await;
    let dag = chain(2);
    let run = create(&store, &dag, "unit_apply_fail", "unit-apply-fail-key").await;
    let running = claim_node(&store, &run, &nid("n0")).await;
    let done = completion(&dag, &running, &nid("n0"));

    // The stale version is refused by `apply_in`, after the work already wrote its document.
    let doc = probe_doc();
    let id = doc.id;
    let mut work = insert_work(doc);
    let result = store
        .commit_with(&done, running.version + 7, &mut work)
        .await;
    match result {
        Err(ExecError::VersionConflict { current, expected }) => {
            assert_eq!(current, running.version);
            assert_eq!(expected, running.version + 7);
        }
        other => panic!("the stale version must be refused, got {other:?}"),
    }

    assert!(
        !document_exists(&backend, &catalog, id).await,
        "the document rolled back with the refused transition"
    );
    let stored = store.load_run(&run.id).await.unwrap().unwrap();
    assert_eq!(stored.node(&nid("n0")).unwrap().status, NodeStatus::Running);
    assert_eq!(stored.version, running.version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_completion_is_a_no_op() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (store, _backend, _catalog) = prepared(&db).await;
    let dag = chain(2);
    let run = create(&store, &dag, "unit_retry", "unit-retry-key").await;
    let running = claim_node(&store, &run, &nid("n0")).await;
    let done = completion(&dag, &running, &nid("n0"));

    let mut first = no_work();
    let once = store
        .commit_with(&done, running.version, &mut first)
        .await
        .expect("the first completion commits");
    assert_eq!(once.version, running.version + 1);

    let mut again = no_work();
    let twice = store
        .commit_with(&done, running.version, &mut again)
        .await
        .expect("the retry commits as a no-op");
    assert_eq!(twice, once, "the retried completion changed nothing");
    assert_eq!(
        store.load_run(&run.id).await.unwrap().unwrap(),
        once,
        "only one completion was stored"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_racing_on_one_run_serialise() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (store, backend, catalog) = prepared(&db).await;
    let dag = chain(2);
    let run = create(&store, &dag, "unit_race", "unit-race-key").await;
    let begin = spot_dag_exec::claim(&run, &nid("n0"), now()).unwrap();
    let expected = run.version;

    let options = || TxOptions::new(n("dag_unit_race"), Isolation::ReadCommitted);
    let left_tx = backend
        .begin(Arc::clone(&catalog), options())
        .await
        .expect("left transaction");
    let right_tx = backend
        .begin(Arc::clone(&catalog), options())
        .await
        .expect("right transaction");

    let one = Arc::new(store);
    let two = Arc::clone(&one);
    let check = Arc::clone(&one);
    let first = begin.clone();
    let second = begin.clone();
    let (first, second) = tokio::join!(
        async move {
            let mut tx = left_tx;
            let out = one.apply_in(&mut tx, &first, expected).await;
            if out.is_ok() {
                tx.commit().await.unwrap();
            } else {
                tx.rollback().await.unwrap();
            }
            out
        },
        async move {
            let mut tx = right_tx;
            let out = two.apply_in(&mut tx, &second, expected).await;
            if out.is_ok() {
                tx.commit().await.unwrap();
            } else {
                tx.rollback().await.unwrap();
            }
            out
        },
    );

    let first = first.expect("the left worker's apply");
    let second = second.expect("the right worker's apply");
    assert_eq!(
        first, second,
        "both workers see exactly one claim: the second is a no-op, not a lost update"
    );
    assert_eq!(first.version, expected + 1, "one claim advanced the run");
    let stored = check.load_run(&run.id).await.unwrap().unwrap();
    assert_eq!(stored.version, expected + 1, "no lost update");
    assert_eq!(stored, first, "the stored run is what both workers saw");
    assert_eq!(stored.node(&nid("n0")).unwrap().attempts, 1);
    assert_eq!(stored.node(&nid("n0")).unwrap().status, NodeStatus::Running);
}
