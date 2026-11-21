//! The replay table carries the `spot_` prefix `spot_pg_migrate` requires, a database that still
//! holds the old name is upgraded without losing a consumed id, and `prepare` is safe to run
//! again and from several nodes at once.
//!
//! Every test here needs a server and skips cleanly (printing `SKIPPED:`) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_sso::{Claim, ReplayStore};
use spot_sso_pg::PgReplayStore;
use spot_storage_contract::{Catalog, Collation};

/// The table as the crate created it before the prefix: the same shape, the old names.
const OLD_DDL: &str = r"
CREATE TABLE sso_replay (
    namespace  text NOT NULL,
    id         text NOT NULL,
    expires_at bigint NOT NULL,
    PRIMARY KEY (namespace, id),
    CONSTRAINT sso_replay_keys CHECK (
        octet_length(namespace) BETWEEN 1 AND 128
        AND octet_length(id) BETWEEN 1 AND 1024)
);
CREATE INDEX sso_replay_expires_idx ON sso_replay (expires_at);
INSERT INTO sso_replay (namespace, id, expires_at)
    VALUES ('upgrade', '14:saml-assertion4:okta5:old-1', 9000000000),
           ('upgrade', '14:saml-assertion4:okta5:old-2', 9000000001);
";

/// Every relation, index, constraint and sequence name of the current schema, sorted.
async fn object_names(db: &TestDb) -> Vec<String> {
    let client = db.raw_client().await;
    client
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
    let client = db.raw_client().await;
    client
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

async fn rows(db: &TestDb, table: &str) -> Vec<(String, String, i64)> {
    db.raw_client()
        .await
        .query(
            &format!("SELECT namespace, id, expires_at FROM {table} ORDER BY namespace, id"),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
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
    let _store = common::store(&db, 2, "upgrade", "sso_prefix").await;
    assert_eq!(table_names(&db).await, vec!["spot_sso_replay".to_owned()]);
    for name in object_names(&db).await {
        let bare = name.split_once(':').unwrap().1;
        assert!(bare.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
    assert_eq!(migrator_refusals(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_named_table_is_renamed_with_every_row_kept() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let before = {
        let client = db.raw_client().await;
        client.batch_execute(OLD_DDL).await.unwrap();
        rows(&db, "sso_replay").await
    };
    assert_eq!(before.len(), 2);

    let store = common::store(&db, 2, "upgrade", "sso_upgrade").await;
    // Byte-for-byte the same rows under the new name, the old name gone.
    assert_eq!(rows(&db, "spot_sso_replay").await, before);
    assert_eq!(table_names(&db).await, vec!["spot_sso_replay".to_owned()]);
    // A consumed id is still consumed: the primary key survived the rename.
    let id = common::namespaced("saml-assertion", "okta", "old-1");
    assert_eq!(store.claim(&id, 100, 200).await.unwrap(), Claim::Replayed);
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
    let _a = common::store(&old, 2, "upgrade", "sso_cat_old").await;
    let _b = common::store(&fresh, 2, "upgrade", "sso_cat_fresh").await;
    let upgraded = object_names(&old).await;
    assert_eq!(upgraded, object_names(&fresh).await);
    assert!(upgraded.iter().any(|n| n == "con:spot_sso_replay_pkey"));
    assert!(upgraded.iter().any(|n| n == "con:spot_sso_replay_keys"));
    assert!(
        upgraded
            .iter()
            .any(|n| n == "idx:spot_sso_replay_expires_idx")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let store = common::store(&db, 2, "upgrade", "sso_again").await;
    let names = object_names(&db).await;
    let data = rows(&db, "spot_sso_replay").await;
    for _ in 0..3 {
        store.prepare().await.unwrap();
    }
    assert_eq!(object_names(&db).await, names);
    assert_eq!(rows(&db, "spot_sso_replay").await, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_preparing_at_once_upgrade_once_and_lose_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let mut tasks = Vec::new();
    for node in 0..6 {
        let store =
            PgReplayStore::new(common::pool(&db, 1, &format!("sso_node_{node}")), "upgrade");
        tasks.push(tokio::spawn(async move { store.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(rows(&db, "spot_sso_replay").await.len(), 2);
    assert_eq!(table_names(&db).await, vec!["spot_sso_replay".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_tables_present_is_refused_and_neither_is_touched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client.batch_execute(OLD_DDL).await.unwrap();
    client
        .batch_execute(
            "CREATE TABLE spot_sso_replay (LIKE sso_replay INCLUDING ALL); \
             INSERT INTO spot_sso_replay VALUES ('upgrade', '14:saml-assertion4:okta5:new-1', 3000000)",
        )
        .await
        .unwrap();
    let before = object_names(&db).await;

    let store = PgReplayStore::new(common::pool(&db, 1, "sso_both"), "upgrade");
    let error = store.prepare().await.expect_err("no merge by guess");
    let text = error.to_string();
    assert!(
        text.contains("`sso_replay`") && text.contains("`spot_sso_replay`"),
        "{text}"
    );
    assert!(!text.contains("old-1") && !text.contains("okta"), "{text}");
    assert_eq!(rows(&db, "sso_replay").await.len(), 2);
    assert_eq!(rows(&db, "spot_sso_replay").await.len(), 1);
    assert_eq!(object_names(&db).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_table_with_the_old_name_is_not_renamed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client
        .batch_execute(
            "CREATE TABLE sso_replay (namespace text NOT NULL, secret text); \
             INSERT INTO sso_replay VALUES ('x', 'do-not-print')",
        )
        .await
        .unwrap();
    let store = PgReplayStore::new(common::pool(&db, 1, "sso_foreign"), "upgrade");
    let error = store
        .prepare()
        .await
        .expect_err("a foreign table is left alone");
    let text = error.to_string();
    assert!(text.contains("`sso_replay`"), "{text}");
    assert!(!text.contains("do-not-print"), "{text}");
    assert_eq!(table_names(&db).await, vec!["sso_replay".to_owned()]);
}
