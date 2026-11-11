//! The tables carry the `spot_` prefix `spot_pg_migrate` requires, a database that still holds
//! the old names is upgraded without losing a row, and `prepare` is safe to run again and from
//! several nodes at once.
//!
//! Every test here needs a server and skips cleanly without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::ExecStore;
use spot_dag_exec::RunId;
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};

/// The tables as the crate created them before the prefix: the same shape, the old names.
const OLD_DDL: &str = r#"
CREATE TABLE dag_run (
    id           text PRIMARY KEY,
    key          text NOT NULL,
    dag_key      text NOT NULL,
    dag_version  integer NOT NULL,
    content_hash text NOT NULL,
    status       text NOT NULL
        CHECK (status IN ('running', 'completed', 'failed', 'cancelled')),
    started_at   timestamptz NOT NULL,
    ended_at     timestamptz,
    version      bigint NOT NULL DEFAULT 0 CHECK (version >= 0),
    taken        jsonb NOT NULL DEFAULT '[]'::jsonb,
    CONSTRAINT dag_run_key_uniq UNIQUE (key)
);
CREATE TABLE dag_node_state (
    run_id     text NOT NULL REFERENCES dag_run (id) ON DELETE CASCADE,
    node       text NOT NULL,
    status     text NOT NULL
        CHECK (status IN ('pending', 'ready', 'running', 'done', 'skipped', 'failed')),
    attempts   integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    output     jsonb,
    failure    text,
    started_at timestamptz,
    ended_at   timestamptz,
    PRIMARY KEY (run_id, node)
);
CREATE INDEX dag_run_ready_idx ON dag_run (id) WHERE status = 'running';
CREATE INDEX dag_node_ready_idx ON dag_node_state (run_id, node) WHERE status = 'ready';
INSERT INTO dag_run VALUES
    ('old_run_1', 'old-key-1', 'chain', 1, 'hash-1', 'running',
     '2023-11-14T22:13:20Z', NULL, 3, '[{"from": "n0", "to": "n1"}]'),
    ('old_run_2', 'old-key-2', 'chain', 1, 'hash-1', 'completed',
     '2023-11-14T22:13:20Z', '2023-11-14T22:14:00Z', 5, '[]');
INSERT INTO dag_node_state VALUES
    ('old_run_1', 'n0', 'done', 1, '{"x": [1, 2, 3]}', NULL, '2023-11-14T22:13:20Z', '2023-11-14T22:13:21Z'),
    ('old_run_1', 'n1', 'ready', 0, NULL, NULL, NULL, NULL),
    ('old_run_2', 'n0', 'failed', 2, NULL, 'boom', '2023-11-14T22:13:20Z', '2023-11-14T22:13:22Z');
"#;

/// Every relation, index and constraint name of the current schema, sorted.
async fn object_names(db: &TestDb) -> Vec<String> {
    db.raw_client()
        .await
        .query(
            "SELECT 'rel:' || relname::text FROM pg_class \
             WHERE relnamespace = to_regnamespace(current_schema())::oid \
             UNION SELECT 'con:' || conname::text FROM pg_constraint \
             WHERE connamespace = to_regnamespace(current_schema())::oid \
             UNION SELECT 'idx:' || c.relname::text FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

async fn table_names(db: &TestDb) -> Vec<String> {
    db.raw_client()
        .await
        .query(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// Every row of both tables as canonical json text, in a fixed order, without the `tenant`
/// column (the upgrade adds it): compares the stored data of the old shape.
async fn dump(db: &TestDb, run: &str, node: &str) -> (Vec<String>, Vec<String>) {
    let client = db.raw_client().await;
    let runs = client
        .query(
            &format!(
                "SELECT (to_jsonb(t) - 'tenant')::text FROM {run} t ORDER BY id COLLATE \"C\""
            ),
            &[],
        )
        .await
        .unwrap();
    let nodes = client
        .query(
            &format!(
                "SELECT (to_jsonb(t) - 'tenant')::text FROM {node} t ORDER BY run_id COLLATE \"C\", node COLLATE \"C\""
            ),
            &[],
        )
        .await
        .unwrap();
    (
        runs.iter().map(|r| r.get(0)).collect(),
        nodes.iter().map(|r| r.get(0)).collect(),
    )
}

async fn migrator_refusals(db: &TestDb) -> usize {
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    plan(&live, &wanted).refused.len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_table_the_crate_creates_is_spot_prefixed_and_passes_the_migrator() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    common::store(&db, 2, "dag_prefix").prepare().await.unwrap();
    assert_eq!(
        table_names(&db).await,
        vec!["spot_dag_node_state".to_owned(), "spot_dag_run".to_owned()]
    );
    for name in object_names(&db).await {
        let bare = name.split_once(':').unwrap().1;
        assert!(bare.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
    assert_eq!(migrator_refusals(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_named_tables_are_renamed_with_every_row_kept() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let before = dump(&db, "dag_run", "dag_node_state").await;
    assert_eq!((before.0.len(), before.1.len()), (2, 3));

    let store = common::store(&db, 2, "dag_upgrade");
    store.prepare().await.unwrap();
    // Byte-for-byte the same rows under the new names, the old names gone.
    assert_eq!(
        dump(&db, "spot_dag_run", "spot_dag_node_state").await,
        before
    );
    assert_eq!(
        table_names(&db).await,
        vec!["spot_dag_node_state".to_owned(), "spot_dag_run".to_owned()]
    );
    // The crate's own API reads them.
    let run = store
        .load_run(&RunId::new("old_run_1").unwrap())
        .await
        .unwrap()
        .expect("run kept");
    assert_eq!(run.key, "old-key-1");
    assert_eq!(run.version, 3);
    assert_eq!(run.nodes.len(), 2);
    assert!(store.find_by_key("old-key-2").await.unwrap().is_some());
    // The run key is still unique: the constraint survived the rename.
    let dag = common::chain(2);
    let plan = spot_dag_exec::start(
        &dag,
        &common::published(&dag),
        RunId::new("fresh").unwrap(),
        "old-key-1",
        common::now(),
    )
    .unwrap();
    assert!(matches!(
        store.create_run(plan).await,
        Err(spot_dag_exec::ExecError::KeyConflict { .. })
    ));
    assert_eq!(migrator_refusals(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgraded_database_has_the_catalogue_of_a_fresh_one() {
    let Some(old) = TestDb::create().await else {
        return;
    };
    let Some(fresh) = TestDb::create().await else {
        return;
    };
    old.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    common::store(&old, 2, "dag_cat_old")
        .prepare()
        .await
        .unwrap();
    common::store(&fresh, 2, "dag_cat_fresh")
        .prepare()
        .await
        .unwrap();
    let upgraded = object_names(&old).await;
    assert_eq!(upgraded, object_names(&fresh).await);
    for expected in [
        "con:spot_dag_run_key_uniq",
        "idx:spot_dag_run_tenant_id_uniq",
        "idx:spot_dag_run_tenant_key_uniq",
        "idx:spot_dag_node_state_tenant_run_node_uniq",
        "con:spot_dag_run_pkey",
        "con:spot_dag_node_state_pkey",
        "con:spot_dag_node_state_run_id_fkey",
        "idx:spot_dag_run_ready_idx",
        "idx:spot_dag_node_ready_idx",
    ] {
        assert!(upgraded.iter().any(|n| n == expected), "missing {expected}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let store = common::store(&db, 2, "dag_again");
    store.prepare().await.unwrap();
    let names = object_names(&db).await;
    let data = dump(&db, "spot_dag_run", "spot_dag_node_state").await;
    for _ in 0..3 {
        store.prepare().await.unwrap();
    }
    assert_eq!(object_names(&db).await, names);
    assert_eq!(dump(&db, "spot_dag_run", "spot_dag_node_state").await, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_preparing_at_once_upgrade_once_and_lose_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let before = dump(&db, "dag_run", "dag_node_state").await;
    let mut tasks = Vec::new();
    for node in 0..6 {
        let store = common::store(&db, 1, &format!("dag_node_{node}"));
        tasks.push(tokio::spawn(async move { store.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(
        dump(&db, "spot_dag_run", "spot_dag_node_state").await,
        before
    );
    assert_eq!(
        table_names(&db).await,
        vec!["spot_dag_node_state".to_owned(), "spot_dag_run".to_owned()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_names_present_is_refused_and_nothing_is_touched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client.batch_execute(OLD_DDL).await.unwrap();
    client
        .batch_execute(
            "CREATE TABLE spot_dag_run (LIKE dag_run INCLUDING ALL); \
             INSERT INTO spot_dag_run SELECT * FROM dag_run WHERE id = 'old_run_1'",
        )
        .await
        .unwrap();
    let names = object_names(&db).await;
    let old_rows = dump(&db, "dag_run", "dag_node_state").await;
    let new_rows = client
        .query("SELECT t::text FROM spot_dag_run t", &[])
        .await
        .unwrap()
        .len();

    let error = common::store(&db, 1, "dag_both")
        .prepare()
        .await
        .expect_err("no merge by guess");
    let text = error.to_string();
    assert!(
        text.contains("`dag_run`") && text.contains("`spot_dag_run`"),
        "{text}"
    );
    assert!(
        !text.contains("old_run") && !text.contains("old-key"),
        "{text}"
    );
    assert_eq!(object_names(&db).await, names);
    assert_eq!(dump(&db, "dag_run", "dag_node_state").await, old_rows);
    assert_eq!(
        client
            .query("SELECT t::text FROM spot_dag_run t", &[])
            .await
            .unwrap()
            .len(),
        new_rows
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_table_with_the_old_name_is_not_renamed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client()
        .await
        .batch_execute(
            "CREATE TABLE dag_run (id text PRIMARY KEY, secret text); \
             INSERT INTO dag_run VALUES ('x', 'do-not-print')",
        )
        .await
        .unwrap();
    let error = common::store(&db, 1, "dag_foreign")
        .prepare()
        .await
        .expect_err("a foreign table is left alone");
    let text = error.to_string();
    assert!(text.contains("`dag_run`"), "{text}");
    assert!(!text.contains("do-not-print"), "{text}");
    // The refusal rolled everything back: no new table was created either.
    assert_eq!(table_names(&db).await, vec!["dag_run".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_table_of_another_schema_is_never_touched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client
        .batch_execute("CREATE SCHEMA other; SET search_path TO other; ")
        .await
        .unwrap();
    // The OLD_DDL goes into `other` only; the crate prepares the connection's own schema.
    client
        .batch_execute(&format!("SET search_path TO other; {OLD_DDL}"))
        .await
        .unwrap();
    common::store(&db, 1, "dag_other_schema")
        .prepare()
        .await
        .unwrap();
    let rows: i64 = client
        .query_one("SELECT count(*) FROM other.dag_run", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 2);
    let renamed: bool = client
        .query_one("SELECT to_regclass('other.spot_dag_run') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    assert!(!renamed);
}

async fn tenant_columns(db: &TestDb) -> (Vec<String>, Vec<String>, Vec<String>) {
    let client = db.raw_client().await;
    let runs = client
        .query("SELECT DISTINCT tenant FROM spot_dag_run ORDER BY 1", &[])
        .await
        .unwrap();
    let nodes = client
        .query(
            "SELECT DISTINCT tenant FROM spot_dag_node_state ORDER BY 1",
            &[],
        )
        .await
        .unwrap();
    let nullable = client
        .query(
            "SELECT table_name::text || ':' || is_nullable::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND column_name = 'tenant' ORDER BY 1",
            &[],
        )
        .await
        .unwrap();
    (
        runs.iter().map(|r| r.get(0)).collect(),
        nodes.iter().map(|r| r.get(0)).collect(),
        nullable.iter().map(|r| r.get(0)).collect(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_rows_are_backfilled_with_the_connections_tenant_and_the_column_is_not_null() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let store = common::store(&db, 2, "dag_tenant_upgrade");
    store.prepare().await.unwrap();
    let tenant = store.tenant().to_owned();
    let (runs, nodes, nullable) = tenant_columns(&db).await;
    assert_eq!(runs, vec![tenant.clone()]);
    assert_eq!(nodes, vec![tenant.clone()]);
    assert_eq!(
        nullable,
        vec![
            "spot_dag_node_state:NO".to_owned(),
            "spot_dag_run:NO".to_owned()
        ]
    );
    // The upgraded rows are readable and advanceable through the tenant-scoped store.
    let run = store
        .load_run(&RunId::new("old_run_1").unwrap())
        .await
        .unwrap()
        .expect("run kept");
    assert_eq!(run.key, "old-key-1");
    // The business key is unique within the tenant: the new unique index is in force.
    let dup = db
        .raw_client()
        .await
        .execute(
            "INSERT INTO spot_dag_run (id, tenant, key, dag_key, dag_version, content_hash, \
             status, started_at, version) VALUES ('x', $1, 'old-key-1', 'c', 1, 'h', 'running', now(), 1)",
            &[&tenant],
        )
        .await;
    assert!(dup.is_err(), "the (tenant, key) unique index must hold");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_again_does_not_touch_the_tenant_of_a_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let store = common::store(&db, 2, "dag_tenant_again");
    store.prepare().await.unwrap();
    // A row stamped for a different tenant is not rewritten by a later prepare.
    db.raw_client()
        .await
        .batch_execute(
            "UPDATE spot_dag_node_state SET tenant = 'elsewhere' WHERE run_id = 'old_run_2'; \
                        UPDATE spot_dag_run SET tenant = 'elsewhere' WHERE id = 'old_run_2'",
        )
        .await
        .unwrap();
    store.prepare().await.unwrap();
    let (runs, nodes, _) = tenant_columns(&db).await;
    assert!(runs.contains(&"elsewhere".to_owned()), "{runs:?}");
    assert!(nodes.contains(&"elsewhere".to_owned()), "{nodes:?}");
}
