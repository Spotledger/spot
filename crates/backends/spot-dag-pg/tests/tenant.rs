//! Tenant isolation of the DAG apply path (WP-DAG-02.T6): every read, update and delete
//! carries the store's tenant, so a run id that belongs to another tenant is `NotFound` and its
//! rows are never changed, even when the caller holds the id or a transaction of another tenant.
//!
//! Every test here needs a server and skips cleanly without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{ExecStore, nid};
use spot_dag_exec::{ExecError, RunId, claim};
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Isolation, StorageBackend, TxOptions};

/// A run and one node row stamped for another tenant, written by raw SQL.
const FOREIGN: &str = r#"
INSERT INTO spot_dag_run (id, tenant, key, dag_key, dag_version, content_hash, status, started_at, version, taken)
    VALUES ('foreign_run', 'tenant_b', 'foreign-key', 'chain', 1, 'hash', 'running', now(), 1, '[]'::jsonb);
INSERT INTO spot_dag_node_state (run_id, tenant, node, status, attempts)
    VALUES ('foreign_run', 'tenant_b', 'n0', 'ready', 0);
"#;

async fn foreign_state(db: &TestDb) -> String {
    db.raw_client()
        .await
        .query_one(
            "SELECT r.version::text || '/' || r.status || '/' || n.status || '/' || n.attempts::text \
             FROM spot_dag_run r JOIN spot_dag_node_state n ON n.run_id = r.id WHERE r.id = 'foreign_run'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_store_stamps_its_tenant_on_every_run_and_node_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = common::store(&db, 2, "dag_tenant_stamp");
    store.prepare().await.unwrap();
    let dag = common::chain(2);
    let run = common::create(&store, &dag, "mine", "mine-key").await;
    assert_eq!(run.id.as_str(), "mine");
    let client = db.raw_client().await;
    let wrong: i64 = client
        .query_one(
            "SELECT (SELECT count(*) FROM spot_dag_run WHERE tenant <> $1) \
                  + (SELECT count(*) FROM spot_dag_node_state WHERE tenant <> $1)",
            &[&store.tenant()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(wrong, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_cannot_read_advance_or_list_another_tenants_run_given_its_id() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = common::store(&db, 2, "dag_tenant_foreign");
    store.prepare().await.unwrap();
    db.raw_client().await.batch_execute(FOREIGN).await.unwrap();
    let before = foreign_state(&db).await;
    let id = RunId::new("foreign_run").unwrap();

    assert!(store.load_run(&id).await.unwrap().is_none());
    assert!(store.find_by_key("foreign-key").await.unwrap().is_none());
    assert!(store.list_ready(10).await.unwrap().is_empty());

    // Build a legitimate-looking claim from a local copy of the same shape and aim it at the id.
    let dag = common::chain(2);
    let local = common::create(&store, &dag, "local", "local-key").await;
    let mut step = claim(&local, &nid("n0"), common::now()).unwrap();
    step.run = id.clone();
    let denied = store.apply(&step, 1).await;
    assert!(
        matches!(denied, Err(ExecError::RunNotFound { .. })),
        "{denied:?}"
    );

    // The foreign rows are byte-for-byte what they were: nothing was advanced or deleted.
    assert_eq!(foreign_state(&db).await, before);
    // The store's own run is still served and listed.
    assert!(store.load_run(&local.id).await.unwrap().is_some());
    assert_eq!(store.list_ready(10).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transaction_of_another_tenant_is_refused_and_writes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let Some(other) = TestDb::create().await else {
        return;
    };
    let store = common::store(&db, 2, "dag_tenant_tx");
    store.prepare().await.unwrap();
    let dag = common::chain(2);
    let run = common::create(&store, &dag, "mine", "mine-key").await;
    let step = claim(&run, &nid("n0"), common::now()).unwrap();

    // A transaction on the other tenant's database, handed to this store's apply_in.
    let foreign_backend = common::backend(&other);
    let options = TxOptions::new(common::n("dag_mixup"), Isolation::ReadCommitted);
    let mut tx = foreign_backend
        .begin(common::catalog(), options)
        .await
        .unwrap();
    let denied = store.apply_in(&mut tx, &step, run.version).await;
    assert!(
        matches!(denied, Err(ExecError::StorageFailed(_))),
        "{denied:?}"
    );
    drop(tx);

    // Nothing moved on either database.
    let reloaded = store.load_run(&run.id).await.unwrap().unwrap();
    assert_eq!(reloaded.version, run.version);
}
