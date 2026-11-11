//! Every table is `spot_`-prefixed, and databases that hold the old names are upgraded in place
//! without losing, reordering or altering an audit row; the hash chain verifies before and after.
//!
//! Every test here needs a server and skips cleanly (printing `SKIPPED:`) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use spot_audit_chain::{
    AuditChain, AuditRecord, CheckpointPolicy, CheckpointSigner, KeyRing, LocalSigner,
};
use spot_audit_store::{PgAuditStore, VerifyJob, prepare};
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};
use tokio_postgres::Client;

/// The tables, role grants and guard as the crate created them before the prefix.
const OLD_DDL: &str = "
CREATE TABLE audit_chain (
    site text NOT NULL, seq bigint NOT NULL, prev_hash bytea NOT NULL, hash bytea NOT NULL,
    record text NOT NULL, at bigint NOT NULL, PRIMARY KEY (site, seq));
CREATE TABLE audit_checkpoint (
    site text PRIMARY KEY, seq bigint NOT NULL, head_hash bytea NOT NULL, at bigint NOT NULL,
    key_id text NOT NULL, signature bytea NOT NULL);
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_audit_app') THEN
    CREATE ROLE spot_audit_app NOLOGIN;
  END IF;
EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
END $$;
GRANT USAGE ON SCHEMA public TO spot_audit_app;
REVOKE UPDATE, DELETE, TRUNCATE ON audit_chain FROM spot_audit_app;
GRANT SELECT, INSERT ON audit_chain TO spot_audit_app;
GRANT SELECT, INSERT, UPDATE ON audit_checkpoint TO spot_audit_app;
CREATE OR REPLACE FUNCTION audit_chain_guard() RETURNS trigger AS $$
BEGIN
  IF current_setting('spot.audit_maintenance', true) = 'on' THEN
    IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF;
  END IF;
  RAISE EXCEPTION 'audit_chain is append-only' USING ERRCODE = '42501';
END $$ LANGUAGE plpgsql;
CREATE TRIGGER audit_chain_no_rewrite BEFORE UPDATE OR DELETE ON audit_chain
  FOR EACH ROW EXECUTE FUNCTION audit_chain_guard();
";

const SITE: &str = "upgrade";

fn record(n: i64) -> AuditRecord {
    AuditRecord {
        kind: "doc.update".to_owned(),
        actor: "user:u1".to_owned(),
        on_behalf_of: None,
        subject: None,
        at: n,
        payload: Default::default(),
        masked: true,
    }
}

fn signer() -> (Arc<LocalSigner>, Arc<dyn CheckpointSigner>) {
    let local = Arc::new(LocalSigner::from_seed([9; 32]));
    let dynamic: Arc<dyn CheckpointSigner> = local.clone();
    (local, dynamic)
}

fn keyring(local: &LocalSigner) -> KeyRing {
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).expect("the key adds");
    keys
}

async fn strings(client: &Client, sql: &str) -> Vec<String> {
    client
        .query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

async fn tables(client: &Client) -> Vec<String> {
    strings(
        client,
        "SELECT table_name::text FROM information_schema.tables \
         WHERE table_schema = current_schema() ORDER BY 1",
    )
    .await
}

/// Every relation, constraint, trigger and function name the crate leaves in the schema.
async fn catalogue(client: &Client) -> Vec<String> {
    strings(
        client,
        "SELECT 'rel ' || c.relkind::text || ' ' || c.relname::text FROM pg_class c \
           WHERE c.relnamespace = to_regnamespace(current_schema())::oid \
         UNION ALL SELECT 'con ' || c.contype::text || ' ' || c.conname::text FROM pg_constraint c \
           WHERE c.connamespace = to_regnamespace(current_schema())::oid \
         UNION ALL SELECT 'idx ' || i.indexrelid::regclass::text FROM pg_index i \
           JOIN pg_class t ON t.oid = i.indrelid \
           WHERE t.relnamespace = to_regnamespace(current_schema())::oid \
         UNION ALL SELECT 'trg ' || g.tgname::text FROM pg_trigger g \
           JOIN pg_class t ON t.oid = g.tgrelid \
           WHERE NOT g.tgisinternal AND t.relnamespace = to_regnamespace(current_schema())::oid \
         UNION ALL SELECT 'fn ' || p.proname::text FROM pg_proc p \
           WHERE p.pronamespace = to_regnamespace(current_schema())::oid \
         ORDER BY 1",
    )
    .await
}

/// Every stored row, whole, with its physical position: equal snapshots mean the rows were
/// neither moved, rewritten nor reordered.
async fn snapshot(client: &Client, table: &str) -> Vec<String> {
    strings(
        client,
        &format!("SELECT t.ctid::text || ' ' || t::text FROM {table} t ORDER BY t.site, t.seq"),
    )
    .await
}

async fn oids(client: &Client) -> Vec<String> {
    strings(
        client,
        "SELECT relname::text || '=' || oid::text FROM pg_class \
         WHERE relnamespace = to_regnamespace(current_schema())::oid ORDER BY 1",
    )
    .await
}

fn pool_of(db: &TestDb) -> deadpool_postgres::Pool {
    common::pool(db, 4)
}

/// A chain of five entries and a checkpoint, built in `source` under the new names, copied row by
/// row into the OLD-named tables of `target`.
async fn old_database_with_chain(source: &TestDb, target: &TestDb) {
    let (_, dynamic) = signer();
    let chain = AuditChain::new(
        common::store(source).await,
        SITE,
        dynamic,
        CheckpointPolicy { every: 1000 },
    );
    for n in 1..=5 {
        chain.append(record(n)).await.expect("append");
    }
    chain.checkpoint_now(100).await.expect("checkpoint");
    let from = common::raw(source).await;
    let rows = from
        .query(
            "SELECT seq, prev_hash, hash, record, at FROM spot_audit_chain ORDER BY seq",
            &[],
        )
        .await
        .unwrap();
    let cps = from
        .query(
            "SELECT seq, head_hash, at, key_id, signature FROM spot_audit_checkpoint",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(cps.len(), 1);

    let to = common::raw(target).await;
    to.batch_execute(OLD_DDL).await.unwrap();
    for row in &rows {
        to.execute(
            "INSERT INTO audit_chain (site, seq, prev_hash, hash, record, at) \
             VALUES ($6, $1, $2, $3, $4, $5)",
            &[
                &row.get::<_, i64>(0),
                &row.get::<_, Vec<u8>>(1),
                &row.get::<_, Vec<u8>>(2),
                &row.get::<_, String>(3),
                &row.get::<_, i64>(4),
                &SITE,
            ],
        )
        .await
        .unwrap();
    }
    for cp in &cps {
        to.execute(
            "INSERT INTO audit_checkpoint (site, seq, head_hash, at, key_id, signature) \
             VALUES ($6, $1, $2, $3, $4, $5)",
            &[
                &cp.get::<_, i64>(0),
                &cp.get::<_, Vec<u8>>(1),
                &cp.get::<_, i64>(2),
                &cp.get::<_, String>(3),
                &cp.get::<_, Vec<u8>>(4),
                &SITE,
            ],
        )
        .await
        .unwrap();
    }
}

async fn verifies(store: PgAuditStore) {
    let (local, dynamic) = signer();
    let job = VerifyJob::new(store, SITE, dynamic, 1000, keyring(&local));
    let outcome = job.run().await.expect("run");
    assert!(
        outcome.is_ok(),
        "the chain does not verify: {:?}",
        outcome.broken
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_object_the_crate_creates_starts_with_spot() {
    let Some(db) = common::db().await else { return };
    let _store = common::store(&db).await;
    let client = common::raw(&db).await;
    assert_eq!(
        tables(&client).await,
        vec![
            "spot_audit_chain".to_owned(),
            "spot_audit_checkpoint".to_owned()
        ]
    );
    for name in catalogue(&client).await {
        let bare = name.rsplit(' ').next().unwrap();
        assert!(bare.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_old_database_keeps_every_row_and_its_chain_still_verifies() {
    let Some(source) = common::db().await else {
        return;
    };
    let Some(db) = common::db().await else { return };
    old_database_with_chain(&source, &db).await;
    let client = common::raw(&db).await;
    let chain_before = snapshot(&client, "audit_chain").await;
    let cp_before = snapshot(&client, "audit_checkpoint").await;
    assert_eq!(chain_before.len(), 5);

    prepare(&pool_of(&db)).await.unwrap();

    let all = tables(&client).await;
    assert_eq!(
        all,
        vec![
            "spot_audit_chain".to_owned(),
            "spot_audit_checkpoint".to_owned()
        ],
        "the old names are gone"
    );
    // The same tuples, in the same places, byte for byte.
    assert_eq!(snapshot(&client, "spot_audit_chain").await, chain_before);
    assert_eq!(snapshot(&client, "spot_audit_checkpoint").await, cp_before);
    verifies(PgAuditStore::new(pool_of(&db))).await;

    // The upgraded store keeps appending to the same chain.
    let (local, dynamic) = signer();
    let chain = AuditChain::new(
        PgAuditStore::new(pool_of(&db)),
        SITE,
        dynamic.clone(),
        CheckpointPolicy { every: 1000 },
    );
    chain.append(record(6)).await.expect("append after upgrade");
    let n: i64 = client
        .query_one("SELECT count(*) FROM spot_audit_chain", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 6);
    let job = VerifyJob::new(
        PgAuditStore::new(pool_of(&db)),
        SITE,
        dynamic,
        1000,
        keyring(&local),
    );
    assert!(job.run().await.unwrap().is_ok());

    // Append-only still holds: the guard trigger and the role grants followed the rename.
    let error = client
        .execute("UPDATE spot_audit_chain SET at = at", &[])
        .await
        .expect_err("refused");
    assert_eq!(error.code().map(|c| c.code()), Some("42501"));
    let privileged: bool = client
        .query_one(
            "SELECT has_table_privilege('spot_audit_app', 'spot_audit_chain', 'UPDATE') \
                 OR has_table_privilege('spot_audit_app', 'spot_audit_chain', 'DELETE') \
                 OR has_table_privilege('spot_audit_app', 'spot_audit_chain', 'TRUNCATE')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!privileged);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgraded_database_has_the_catalogue_of_a_fresh_one() {
    let Some(source) = common::db().await else {
        return;
    };
    let Some(old) = common::db().await else {
        return;
    };
    let Some(fresh) = common::db().await else {
        return;
    };
    old_database_with_chain(&source, &old).await;
    prepare(&pool_of(&old)).await.unwrap();
    let _fresh_store = common::store(&fresh).await;

    let upgraded = catalogue(&common::raw(&old).await).await;
    let created = catalogue(&common::raw(&fresh).await).await;
    assert!(!created.is_empty());
    assert_eq!(upgraded, created);
    // The old trigger and function are gone, the new ones are there.
    assert!(created.contains(&"trg spot_audit_chain_no_rewrite".to_owned()));
    assert!(created.contains(&"fn spot_audit_chain_guard".to_owned()));
    assert!(created.contains(&"con p spot_audit_chain_pkey".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_prepare_changes_nothing() {
    let Some(source) = common::db().await else {
        return;
    };
    let Some(db) = common::db().await else { return };
    old_database_with_chain(&source, &db).await;
    let pool = pool_of(&db);
    prepare(&pool).await.unwrap();
    let client = common::raw(&db).await;
    let names = catalogue(&client).await;
    let ids = oids(&client).await;
    let rows = snapshot(&client, "spot_audit_chain").await;

    prepare(&pool).await.unwrap();

    assert_eq!(catalogue(&client).await, names);
    assert_eq!(snapshot(&client, "spot_audit_chain").await, rows);
    // Same relations, not dropped and re-created.
    let after = oids(&client).await;
    let stable = |v: &[String]| -> Vec<String> {
        v.iter()
            .filter(|s| {
                s.starts_with("spot_audit_chain=") || s.starts_with("spot_audit_checkpoint=")
            })
            .cloned()
            .collect()
    };
    assert_eq!(stable(&after), stable(&ids));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_names_present_is_refused_and_both_tables_are_untouched() {
    let Some(source) = common::db().await else {
        return;
    };
    let Some(db) = common::db().await else { return };
    old_database_with_chain(&source, &db).await;
    let client = common::raw(&db).await;
    // The new name of one pair also exists, with other content.
    client
        .batch_execute(
            "CREATE TABLE spot_audit_checkpoint (x int); INSERT INTO spot_audit_checkpoint VALUES (7);",
        )
        .await
        .unwrap();
    let before = tables(&client).await;
    let cat_before = catalogue(&client).await;
    let chain_before = snapshot(&client, "audit_chain").await;

    let error = prepare(&pool_of(&db)).await.unwrap_err();
    assert_eq!(error.code, spot_audit_store::UNAVAILABLE);
    assert!(
        error.message.contains("`audit_checkpoint`")
            && error.message.contains("`spot_audit_checkpoint`"),
        "{}",
        error.message
    );
    assert_eq!(tables(&client).await, before, "nothing renamed or dropped");
    assert_eq!(catalogue(&client).await, cat_before);
    assert_eq!(snapshot(&client, "audit_chain").await, chain_before);
    let n: i64 = client
        .query_one("SELECT count(*) FROM spot_audit_checkpoint", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_table_with_an_old_name_is_not_renamed() {
    let Some(db) = common::db().await else { return };
    let client = common::raw(&db).await;
    client
        .batch_execute(
            "CREATE TABLE audit_chain (a int, b int); INSERT INTO audit_chain VALUES (1, 2)",
        )
        .await
        .unwrap();
    let error = prepare(&pool_of(&db)).await.unwrap_err();
    assert_eq!(error.code, spot_audit_store::UNAVAILABLE);
    assert!(error.message.contains("`audit_chain`"));
    assert!(!error.message.contains('1'), "no row data in the message");
    assert_eq!(tables(&client).await, vec!["audit_chain".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_prepares_on_an_old_database_leave_one_correct_schema() {
    let Some(source) = common::db().await else {
        return;
    };
    let Some(db) = common::db().await else { return };
    let Some(fresh) = common::db().await else {
        return;
    };
    old_database_with_chain(&source, &db).await;
    let _fresh_store = common::store(&fresh).await;
    let pool = pool_of(&db);
    let (a, b, c) = tokio::join!(prepare(&pool), prepare(&pool), prepare(&pool));
    a.unwrap();
    b.unwrap();
    c.unwrap();
    let client = common::raw(&db).await;
    assert_eq!(
        catalogue(&client).await,
        catalogue(&common::raw(&fresh).await).await
    );
    verifies(PgAuditStore::new(pool_of(&db))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migrator_does_not_refuse_the_audit_tables() {
    let Some(source) = common::db().await else {
        return;
    };
    let Some(db) = common::db().await else { return };
    old_database_with_chain(&source, &db).await;
    prepare(&pool_of(&db)).await.unwrap();

    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    let plan = plan(&live, &wanted);
    assert!(
        plan.refused.is_empty(),
        "the migrator refuses the audit tables: {:?}",
        plan.refused
    );
}
