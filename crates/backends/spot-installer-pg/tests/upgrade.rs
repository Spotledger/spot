//! The tables carry the `spot_` prefix `spot_pg_migrate` requires, a database that still holds the
//! old names is upgraded without losing a row, and `prepare` is safe to run again and from several
//! nodes at once.
//!
//! Every test here needs a server and skips cleanly without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{install_change, registry, row, site};
use spot_installer::{AppRegistryStore, StepState};
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};

/// The tables as the crate created them before the prefix: the same shape, the old names.
const OLD_DDL: &str = r#"
CREATE TABLE installer_site (
    site      text PRIMARY KEY,
    lifecycle text NOT NULL CHECK (lifecycle IN ('idle', 'installing'))
);
CREATE TABLE installer_app (
    site            text NOT NULL,
    app             text NOT NULL,
    version         text NOT NULL,
    digest          text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    approved        jsonb NOT NULL,
    applied_patches jsonb NOT NULL,
    ord             integer NOT NULL CHECK (ord >= 0),
    depends         jsonb NOT NULL,
    PRIMARY KEY (site, app)
);
CREATE TABLE installer_journal (
    site       text NOT NULL,
    step_index integer NOT NULL CHECK (step_index >= 0),
    plan_hash  text NOT NULL,
    kind       text NOT NULL
        CHECK (kind IN ('snapshot', 'patch', 'fixture', 'remove_fixture',
                        'commit', 'load', 'unload', 'smoke')),
    name       text NOT NULL,
    state      text NOT NULL CHECK (state IN ('started', 'done', 'undone')),
    started_at bigint NOT NULL,
    undo       jsonb,
    PRIMARY KEY (site, step_index)
);
CREATE TABLE installer_owned_doctype (
    site    text NOT NULL,
    app     text NOT NULL,
    doctype text NOT NULL,
    PRIMARY KEY (site, app, doctype)
);
INSERT INTO installer_site VALUES ('old-site', 'installing');
INSERT INTO installer_app VALUES
    ('old-site', 'crm', '1.2.3', repeat('ab', 32), '{"k": [1, 2]}', '["p1"]', 0, '[]');
INSERT INTO installer_journal VALUES
    ('old-site', 0, 'plan-x', 'commit', 'crm', 'done', 1700000000, '{"u": true}');
INSERT INTO installer_owned_doctype VALUES ('old-site', 'crm', 'Customer'), ('old-site', 'crm', 'Lead');
"#;

const OLD_TABLES: [&str; 4] = [
    "installer_site",
    "installer_app",
    "installer_journal",
    "installer_owned_doctype",
];

/// Every relation, index and constraint name of the current schema, sorted.
async fn object_names(db: &TestDb) -> Vec<String> {
    db.raw_client()
        .await
        .query(
            "SELECT n FROM (SELECT 'rel:' || relname::text AS n FROM pg_class \
             WHERE relnamespace = to_regnamespace(current_schema())::oid \
             UNION SELECT 'con:' || conname::text FROM pg_constraint \
             WHERE connamespace = to_regnamespace(current_schema())::oid \
             UNION SELECT 'idx:' || c.relname::text FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid) names \
             ORDER BY n COLLATE \"C\"",
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
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' \
             ORDER BY table_name::text COLLATE \"C\"",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

fn new_table_names() -> Vec<String> {
    let mut names: Vec<String> = OLD_TABLES.iter().map(|t| format!("spot_{t}")).collect();
    names.sort();
    names
}

/// Every row of the four tables as text, in a fixed order, under the given name prefix.
async fn dump(db: &TestDb, prefix: &str) -> Vec<String> {
    let client = db.raw_client().await;
    let mut out = Vec::new();
    for (table, order) in [
        ("installer_site", "site"),
        ("installer_app", "site, app"),
        ("installer_journal", "site, step_index"),
        ("installer_owned_doctype", "site, app, doctype"),
    ] {
        let rows = client
            .query(
                &format!("SELECT t::text FROM {prefix}{table} t ORDER BY {order}"),
                &[],
            )
            .await
            .unwrap();
        out.extend(
            rows.iter()
                .map(|r| format!("{table}|{}", r.get::<_, String>(0))),
        );
    }
    out
}

async fn migrator_refusals(db: &TestDb) -> usize {
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    plan(&live, &wanted).refused.len()
}

/// A database holding old-named tables with rows written through the crate's own API (so every
/// stored value is a real one), by preparing the new schema and renaming it back.
async fn old_database_with_real_rows(db: &TestDb) {
    let store = registry(db, 2, "seed").await;
    let s = site("seeded");
    store
        .commit_install(&s, "crm", install_change(7), row("crm", 1, StepState::Done))
        .await
        .unwrap();
    store
        .register_owned_doctypes(&s, "crm", &["Customer".to_owned(), "Lead".to_owned()])
        .await
        .unwrap();
    let client = db.raw_client().await;
    for table in OLD_TABLES {
        let new = format!("spot_{table}");
        let constraints = client
            .query(
                "SELECT conname::text FROM pg_constraint \
                 WHERE conrelid = to_regclass($1)::oid ORDER BY 1",
                &[&new],
            )
            .await
            .unwrap();
        for c in constraints {
            let name: String = c.get(0);
            let old_name = name.strip_prefix("spot_").unwrap().to_owned();
            client
                .batch_execute(&format!(
                    "ALTER TABLE {new} RENAME CONSTRAINT {name} TO {old_name}"
                ))
                .await
                .unwrap();
        }
        client
            .batch_execute(&format!("ALTER TABLE {new} RENAME TO {table}"))
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_table_the_crate_creates_is_spot_prefixed_and_passes_the_migrator() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    registry(&db, 2, "inst_prefix").await;
    assert_eq!(table_names(&db).await, new_table_names());
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
    old_database_with_real_rows(&db).await;
    db.raw_client()
        .await
        .batch_execute("INSERT INTO installer_site VALUES ('other', 'idle')")
        .await
        .unwrap();
    let before = dump(&db, "").await;
    assert!(before.len() >= 5, "{before:?}");

    let store = registry(&db, 2, "inst_upgrade").await;
    // Byte-for-byte the same rows under the new names, the old names gone.
    assert_eq!(dump(&db, "spot_").await, before);
    assert_eq!(table_names(&db).await, new_table_names());
    // The crate's own API reads them.
    let s = site("seeded");
    let state = store.load(&s).await.unwrap();
    assert_eq!(state.apps.len(), 1);
    assert_eq!(state.apps[0].name, "crm");
    assert_eq!(store.journal(&s).await.unwrap().len(), 1);
    assert_eq!(
        store.owned_doctypes(&s, "crm").await.unwrap(),
        vec!["Customer".to_owned(), "Lead".to_owned()]
    );
    assert_eq!(migrator_refusals(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_raw_old_schema_is_upgraded_too() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let before = dump(&db, "").await;
    registry(&db, 2, "inst_raw").await;
    assert_eq!(dump(&db, "spot_").await, before);
    assert_eq!(table_names(&db).await, new_table_names());
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
    registry(&old, 2, "inst_cat_old").await;
    registry(&fresh, 2, "inst_cat_fresh").await;
    let upgraded = object_names(&old).await;
    assert_eq!(upgraded, object_names(&fresh).await);
    for expected in [
        "con:spot_installer_site_pkey",
        "con:spot_installer_app_pkey",
        "con:spot_installer_journal_pkey",
        "con:spot_installer_owned_doctype_pkey",
        "con:spot_installer_site_lifecycle_check",
        "con:spot_installer_app_digest_check",
        "con:spot_installer_journal_kind_check",
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
    let store = registry(&db, 2, "inst_again").await;
    let names = object_names(&db).await;
    let data = dump(&db, "spot_").await;
    for _ in 0..3 {
        store.prepare().await.unwrap();
    }
    assert_eq!(object_names(&db).await, names);
    assert_eq!(dump(&db, "spot_").await, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_preparing_at_once_upgrade_once_and_lose_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let before = dump(&db, "").await;
    let mut tasks = Vec::new();
    for node in 0..6 {
        let pool = common::pool(&db, 1, &format!("inst_node_{node}"));
        tasks.push(tokio::spawn(async move {
            spot_installer_pg::schema::prepare(&pool).await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(dump(&db, "spot_").await, before);
    assert_eq!(table_names(&db).await, new_table_names());
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
            "CREATE TABLE spot_installer_app (LIKE installer_app INCLUDING ALL); \
             INSERT INTO spot_installer_app SELECT * FROM installer_app",
        )
        .await
        .unwrap();
    let names = object_names(&db).await;
    let old_rows = dump(&db, "").await;
    let new_rows: Vec<String> = client
        .query("SELECT t::text FROM spot_installer_app t", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();

    let pool = common::pool(&db, 1, "inst_both");
    let error = spot_installer_pg::schema::prepare(&pool)
        .await
        .expect_err("no merge by guess");
    let text = error.to_string();
    assert!(
        text.contains("`installer_app`") && text.contains("`spot_installer_app`"),
        "{text}"
    );
    assert!(
        !text.contains("old-site") && !text.contains("crm"),
        "{text}"
    );
    assert_eq!(object_names(&db).await, names);
    assert_eq!(dump(&db, "").await, old_rows);
    let after: Vec<String> = client
        .query("SELECT t::text FROM spot_installer_app t", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(after, new_rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_table_with_the_old_name_is_not_renamed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client()
        .await
        .batch_execute(
            "CREATE TABLE installer_app (id text PRIMARY KEY, secret text); \
             INSERT INTO installer_app VALUES ('x', 'do-not-print')",
        )
        .await
        .unwrap();
    let pool = common::pool(&db, 1, "inst_foreign");
    let error = spot_installer_pg::schema::prepare(&pool)
        .await
        .expect_err("a foreign table is left alone");
    let text = error.to_string();
    assert!(text.contains("`installer_app`"), "{text}");
    assert!(!text.contains("do-not-print"), "{text}");
    // The refusal rolled everything back: no new table was created either.
    assert_eq!(table_names(&db).await, vec!["installer_app".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_taken_target_name_is_refused_and_nothing_is_renamed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client.batch_execute(OLD_DDL).await.unwrap();
    // A foreign relation already holds the name the primary key would take.
    client
        .batch_execute("CREATE TABLE spot_installer_site_pkey (x int)")
        .await
        .unwrap();
    let names = object_names(&db).await;
    let pool = common::pool(&db, 1, "inst_taken");
    let error = spot_installer_pg::schema::prepare(&pool)
        .await
        .expect_err("a taken name is never overwritten");
    assert!(error.to_string().contains("spot_installer_site_pkey"));
    assert_eq!(object_names(&db).await, names);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_table_of_another_schema_is_never_touched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client
        .batch_execute(&format!(
            "CREATE SCHEMA other; SET search_path TO other; {OLD_DDL}"
        ))
        .await
        .unwrap();
    // The OLD_DDL went into `other` only; the crate prepares the pool connection's own schema.
    let pool = common::pool(&db, 1, "inst_other_schema");
    spot_installer_pg::schema::prepare(&pool).await.unwrap();
    let rows: i64 = client
        .query_one("SELECT count(*) FROM other.installer_app", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 1);
    let renamed: bool = client
        .query_one(
            "SELECT to_regclass('other.spot_installer_app') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!renamed);
}
