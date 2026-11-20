//! What a sandbox clone does with framework tables (WP-ALM-05.T4): `Empty` and `Rebuild` tables
//! are truncated, `Keep` tables stay, an unknown table is emptied and reported, document tables
//! are left to masking, and the source site is untouched. Real PostgreSQL through
//! `SPOT_TEST_PG_URL` (skipped with a printed reason when unset).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use spot_meta::{Definition, FieldSpec, FieldType, MaskSpec};
use spot_postgres::testing::TestDb;

/// The command modules, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::sandbox::RECORD_FILE;
use commands::sandbox::db::{mask_clone, verify_clone};

const NAME: &str = "Alice Example";

/// Framework tables of every treatment, with the shape this test gives them in a plain database.
const SEEDED: [(&str, &str); 8] = [
    ("spot_audit_chain", "empty"),
    ("spot_journal_subject_key", "empty"),
    ("spot_signed_command_nonce", "empty"),
    ("spot_job", "empty"),
    ("spot_mail_sent_log", "empty"),
    ("spot_search_doc", "rebuild"),
    ("spot_node_role_state", "empty"),
    ("spot_meta", "keep"),
];

fn definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    let mut name = FieldSpec::new("name", FieldType::Text);
    name.classification = vec!["pii".to_owned()];
    name.protection_group = Some("personal".to_owned());
    name.masking = Some(MaskSpec::placeholder());
    customer.fields = vec![name];
    vec![customer]
}

async fn seed(db: &TestDb) {
    let client = db.raw_client().await;
    let mut sql = String::from(
        "CREATE TABLE customer (id int PRIMARY KEY, name text);\
         INSERT INTO customer VALUES (1, 'Alice Example');\
         CREATE SCHEMA spot_migration_control;\
         CREATE TABLE spot_migration_control.gate (id int, reason text);\
         INSERT INTO spot_migration_control.gate VALUES (1, 'closed on the source');\
         CREATE TABLE spot_test_unknown (id int, note text);\
         INSERT INTO spot_test_unknown VALUES (1, 'a table nobody classified');",
    );
    for (table, _) in SEEDED {
        sql.push_str(&format!(
            "CREATE TABLE {table} (id int, body text); \
             INSERT INTO {table} VALUES (1, 'a row'), (2, 'another');"
        ));
    }
    client.batch_execute(&sql).await.unwrap();
}

async fn count(db: &TestDb, table: &str) -> i64 {
    db.raw_client()
        .await
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_registry_decides_every_framework_table() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    seed(&db).await;
    let (stats, index) = mask_clone(&db.config(), db.name(), &definitions(), [9_u8; 32])
        .await
        .unwrap();

    for (table, treatment) in SEEDED {
        let rows = count(&db, table).await;
        match treatment {
            "keep" => assert_eq!(rows, 2, "{table} is Keep and stays"),
            _ => assert_eq!(rows, 0, "{table} is {treatment} and is emptied"),
        }
    }
    // The control schema's gate is emptied: a copied closed gate would close the clone.
    assert_eq!(count(&db, "spot_migration_control.gate").await, 0);
    // An unknown table is emptied, never kept by silence, and reported.
    assert_eq!(count(&db, "spot_test_unknown").await, 0);
    assert_eq!(stats.unclassified, ["spot_test_unknown"]);
    // The counts add up: 6 Empty seeded + the gate + the unknown table, 1 Rebuild, 1 Keep.
    assert_eq!(stats.emptied, 8, "{stats:?}");
    assert_eq!(stats.rebuilt, 1);
    assert_eq!(stats.kept, 1);
    assert_eq!(stats.purged.len(), 9);
    // The document table is governed by masking: its row is there, masked.
    let masked: String = db
        .raw_client()
        .await
        .query_one("SELECT name FROM customer WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_ne!(masked, NAME);
    // And the leak scan finds no original value anywhere, including the truncated tables.
    verify_clone(&db.config(), db.name(), &index).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_value_seeded_in_a_truncated_table_is_not_a_leak() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    seed(&db).await;
    db.raw_client()
        .await
        .batch_execute("INSERT INTO spot_audit_chain VALUES (3, 'Alice Example signed in')")
        .await
        .unwrap();
    let (_, index) = mask_clone(&db.config(), db.name(), &definitions(), [5_u8; 32])
        .await
        .unwrap();
    assert!(verify_clone(&db.config(), db.name(), &index).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_document_value_that_survives_masking_still_fails_the_scan() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    seed(&db).await;
    let (_, index) = mask_clone(&db.config(), db.name(), &definitions(), [6_u8; 32])
        .await
        .unwrap();
    // A copy of a masked value in a table the registry keeps: the scan still refuses the clone.
    db.raw_client()
        .await
        .batch_execute("UPDATE spot_meta SET body = 'Alice Example' WHERE id = 1")
        .await
        .unwrap();
    let message = verify_clone(&db.config(), db.name(), &index)
        .await
        .expect_err("a surviving original value is refused");
    assert!(message.contains("spot_meta.body"), "{message}");
    assert!(!message.contains(NAME), "the message never carries a value");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_source_site_is_untouched_by_create() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let source = env.site.clone();
    let _secret = env.site();
    let loaded = spot_cli::config::load(&root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == source).unwrap();
    let cfg = loaded.admin.clone().with_dbname(&site.database);
    let backend = spot_postgres::PgBackend::connect(cfg).await.unwrap();
    let client = backend.pool().get().await.unwrap();
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS spot_test_unknown (id int, note text); \
             INSERT INTO spot_test_unknown VALUES (1, 'kept in the source')",
        )
        .await
        .unwrap();
    let tables = ["spot_test_unknown", "spot_meta"];
    let mut before = Vec::new();
    for table in tables {
        let n: i64 = client
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .unwrap()
            .get(0);
        before.push(n);
    }

    let name = format!("{source}-tables");
    let created = common::run(
        &root,
        &[
            "sandbox", "create", "--from", &source, "--name", &name, "--ttl", "1h",
        ],
        None,
    );
    let out = String::from_utf8_lossy(&created.stdout).into_owned();
    assert!(
        created.status.success(),
        "{out}{}",
        String::from_utf8_lossy(&created.stderr)
    );
    // The run says which table it did not know and emptied.
    assert!(out.contains("unclassified_table"), "{out}");
    assert!(out.contains("spot_test_unknown"), "{out}");

    for (table, was) in tables.iter().zip(&before) {
        let n: i64 = client
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, *was, "{table} of the source changed");
    }
    // The clone's database has the unknown table empty.
    let record = commands::sandbox::read_record(&root.join("sites").join(&name))
        .unwrap()
        .unwrap();
    let mut clone_cfg = loaded.admin.clone().with_dbname(&record.database);
    clone_cfg.pool_max_size = 2;
    let clone = spot_postgres::PgBackend::connect(clone_cfg).await.unwrap();
    let clone_client = clone.pool().get().await.unwrap();
    let n: i64 = clone_client
        .query_one("SELECT count(*) FROM spot_test_unknown", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0);
    common::ok(&root, &["sandbox", "destroy", "--name", &name]);
    assert!(!root.join("sites").join(&name).join(RECORD_FILE).exists());
}
