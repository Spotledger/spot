//! Creating the tables through the migrator's plan, then installing and verifying the guard
//! (WP-AI-11.T4). Every test needs a server and skips cleanly (printing `SKIPPED:`) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_audit_store::{
    CHAIN_TABLE, PROTECTION_INCOMPLETE, PgAuditStore, TABLE_DDL, TABLES_MISSING, install_guard,
    prepare, verify,
};
use spot_pg_migrate::{ApplyOptions, ExtraTable, PlanOptions, apply, dry_run};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};
use tokio_postgres::Client;

fn options() -> PlanOptions {
    PlanOptions {
        extra_tables: TABLE_DDL
            .iter()
            .map(|(table, statement)| {
                ExtraTable::new(*table, vec![(*statement).to_owned()]).unwrap()
            })
            .collect(),
        ..PlanOptions::default()
    }
}

fn empty_catalog() -> Catalog {
    Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap()
}

/// Plans the two audit tables through the migrator and applies the plan. Returns the step keys.
async fn migrate(db: &TestDb) -> Vec<String> {
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let plan = dry_run(&backend, &empty_catalog(), &options())
        .await
        .unwrap();
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    apply(&backend, &plan, ApplyOptions::default())
        .await
        .unwrap();
    plan.steps.iter().map(|s| s.key.clone()).collect()
}

/// Columns and indexes of the two tables, as text, for a before/after comparison.
async fn shape(client: &Client) -> Vec<String> {
    let mut out: Vec<String> = client
        .query(
            "SELECT table_name || '.' || column_name || ':' || data_type || ':' || is_nullable \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('spot_audit_chain', 'spot_audit_checkpoint') \
             ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    out.extend(
        client
            .query(
                "SELECT indexdef FROM pg_indexes WHERE schemaname = current_schema() \
                 AND tablename IN ('spot_audit_chain', 'spot_audit_checkpoint') ORDER BY 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0)),
    );
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migrator_plans_the_tables_once_and_prepare_changes_nothing_after() {
    let Some(db) = common::db().await else { return };
    let first = migrate(&db).await;
    assert_eq!(
        first,
        [
            "extra_table:spot_audit_chain",
            "extra_table:spot_audit_checkpoint",
            "finalise"
        ]
    );
    let client = common::raw(&db).await;
    let planned = shape(&client).await;
    assert!(!planned.is_empty());

    // The second plan is empty.
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let again = dry_run(&backend, &empty_catalog(), &options())
        .await
        .unwrap();
    assert!(again.is_empty(), "{:?}", again.steps);

    // prepare after the plan creates no table and changes no column or index.
    prepare(&common::pool(&db, 4)).await.unwrap();
    assert_eq!(shape(&client).await, planned);
    assert!(verify(&common::pool(&db, 2)).await.unwrap().is_complete());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_populated_database_plans_nothing_and_prepare_changes_no_table() {
    let Some(db) = common::db().await else { return };
    // The old way: prepare creates everything and a row lands.
    let store = common::store(&db).await;
    let client = common::raw(&db).await;
    client
        .execute(
            "INSERT INTO spot_audit_chain (site, seq, prev_hash, hash, record, at) \
             VALUES ('s', 1, '\\x00', '\\x01', '{}', 1)",
            &[],
        )
        .await
        .unwrap();
    let before = shape(&client).await;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let plan = dry_run(&backend, &empty_catalog(), &options())
        .await
        .unwrap();
    assert!(plan.is_empty(), "{:?}", plan.steps);
    store.prepare().await.unwrap();
    assert_eq!(shape(&client).await, before);
    let n: i64 = client
        .query_one("SELECT count(*) FROM spot_audit_chain", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_public_ddl_is_what_prepare_creates() {
    let Some(db) = common::db().await else { return };
    // Created only from the public constants.
    let client = common::raw(&db).await;
    for (_, statement) in TABLE_DDL {
        client.batch_execute(statement).await.unwrap();
    }
    let from_constants = shape(&client).await;
    // The same statements again are a no-op, and prepare adds no column or index.
    prepare(&common::pool(&db, 2)).await.unwrap();
    assert_eq!(shape(&client).await, from_constants);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tables_without_the_guard_are_incomplete_until_install_guard_runs() {
    let Some(db) = common::db().await else { return };
    migrate(&db).await;
    let pool = common::pool(&db, 4);
    let client = common::raw(&db).await;
    client
        .execute(
            "INSERT INTO spot_audit_chain (site, seq, prev_hash, hash, record, at) \
             VALUES ('g', 1, '\\x00', '\\x01', '{}', 1)",
            &[],
        )
        .await
        .unwrap();

    let found = verify(&pool).await.unwrap();
    assert!(found.chain_table && found.checkpoint_table);
    assert!(!found.rewrite_trigger && !found.truncate_trigger);
    assert!(!found.is_complete());
    let error = found.require_complete().unwrap_err();
    assert_eq!(error.code, PROTECTION_INCOMPLETE);
    assert!(error.message.contains("spot_audit_chain_no_rewrite"));
    // The unguarded chain can be rewritten: this is the window the host must refuse to serve in.
    assert_eq!(
        client
            .execute("UPDATE spot_audit_chain SET at = 2 WHERE site = 'g'", &[])
            .await
            .unwrap(),
        1
    );

    install_guard(&pool).await.unwrap();
    install_guard(&pool).await.unwrap();
    let found = verify(&pool).await.unwrap();
    assert!(found.is_complete(), "{found:?}");
    found.require_complete().unwrap();

    // The owner is refused by the trigger.
    for sql in [
        "UPDATE spot_audit_chain SET at = 3 WHERE site = 'g'",
        "DELETE FROM spot_audit_chain WHERE site = 'g'",
        "TRUNCATE spot_audit_chain",
    ] {
        let error = client.batch_execute(sql).await.expect_err("refused");
        assert_eq!(error.code().map(|c| c.code()), Some("42501"), "{sql}");
    }
    // The application role is refused by its grants.
    client
        .batch_execute("SET ROLE spot_audit_app")
        .await
        .unwrap();
    for sql in [
        "UPDATE spot_audit_chain SET at = 3 WHERE site = 'g'",
        "DELETE FROM spot_audit_chain WHERE site = 'g'",
        "TRUNCATE spot_audit_chain",
    ] {
        let error = client.batch_execute(sql).await.expect_err("refused");
        assert_eq!(error.code().map(|c| c.code()), Some("42501"), "{sql}");
    }
    client.batch_execute("RESET ROLE").await.unwrap();

    // The maintenance setting is the only way through for the owner.
    client
        .batch_execute("SET spot.audit_maintenance = 'on'")
        .await
        .unwrap();
    client
        .batch_execute("UPDATE spot_audit_chain SET at = 3 WHERE site = 'g'")
        .await
        .unwrap();
    client
        .batch_execute("TRUNCATE spot_audit_chain")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_guard_without_the_tables_fails_and_creates_nothing() {
    let Some(db) = common::db().await else { return };
    let pool = common::pool(&db, 2);
    let error = install_guard(&pool).await.unwrap_err();
    assert_eq!(error.code, TABLES_MISSING);
    assert!(error.message.contains("spot_audit_chain"));
    let client = common::raw(&db).await;
    let n: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_proc WHERE proname = 'spot_audit_chain_guard'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0, "the guard function was created");
    assert!(shape(&client).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_on_an_empty_database_reports_everything_missing_and_changes_nothing() {
    let Some(db) = common::db().await else { return };
    let client = common::raw(&db).await;
    let objects = || async {
        let n: i64 = client
            .query_one(
                "SELECT (SELECT count(*) FROM pg_class WHERE relnamespace = current_schema()::regnamespace) \
                      + (SELECT count(*) FROM pg_proc WHERE proname = 'spot_audit_chain_guard')",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        n
    };
    let before = objects().await;
    let found = verify(&common::pool(&db, 2)).await.unwrap();
    assert!(!found.chain_table && !found.checkpoint_table);
    assert!(!found.rewrite_trigger && !found.truncate_trigger);
    assert!(!found.role_cannot_rewrite);
    assert!(!found.is_complete());
    assert_eq!(objects().await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_trigger_or_a_widened_grant_is_reported() {
    let Some(db) = common::db().await else { return };
    let store = common::store(&db).await;
    let pool = store.pool().clone();
    let client = common::raw(&db).await;
    assert!(verify(&pool).await.unwrap().is_complete());

    client
        .batch_execute("ALTER TABLE spot_audit_chain DISABLE TRIGGER spot_audit_chain_no_rewrite")
        .await
        .unwrap();
    let found = verify(&pool).await.unwrap();
    assert!(!found.rewrite_trigger && found.truncate_trigger);
    assert!(!found.is_complete());
    // install_guard repairs a disabled trigger (it recreates it, enabled).
    install_guard(&pool).await.unwrap();
    assert!(verify(&pool).await.unwrap().is_complete());

    client
        .batch_execute(&format!("GRANT UPDATE ON {CHAIN_TABLE} TO spot_audit_app"))
        .await
        .unwrap();
    let found = verify(&pool).await.unwrap();
    assert!(!found.role_cannot_rewrite);
    assert!(!found.is_complete());
    install_guard(&pool).await.unwrap();
    assert!(verify(&pool).await.unwrap().is_complete());
    let _ = PgAuditStore::new(pool);
}
