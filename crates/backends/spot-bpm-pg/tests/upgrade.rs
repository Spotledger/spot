//! The tables carry the `spot_` prefix `spot_pg_migrate` requires, a database that still holds the
//! old names is upgraded without losing a Case, its variables or its node outcomes, and `prepare`
//! is safe to run again and from several nodes at once.
//!
//! Every test here needs a server and skips cleanly (printing `SKIPPED:`) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::json;
use spot_bpm_engine::{Attempt, AttemptState, CaseId, CaseRecord, CaseStore, CaseUpdate};
use spot_bpm_pg::PgCaseStore;
use spot_dag_exec::Instant;
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};

/// The schema as the crate created it before the prefix: the same shape, the old names.
const OLD_DDL: &str = r"
CREATE TABLE bpm_case (
    case_id      text PRIMARY KEY,
    tenant       text NOT NULL,
    business_key text NOT NULL,
    dag_key      text NOT NULL,
    dag_version  integer NOT NULL,
    content_hash text NOT NULL,
    status       text NOT NULL
        CHECK (status IN ('running', 'completed', 'failed', 'cancelled')),
    version      bigint NOT NULL CHECK (version >= 0),
    started_at   timestamptz NOT NULL,
    ended_at     timestamptz,
    initiator    text,
    nodes        jsonb NOT NULL,
    taken        jsonb NOT NULL DEFAULT '[]'::jsonb,
    children     jsonb NOT NULL DEFAULT '{}'::jsonb,
    has_ready    boolean NOT NULL DEFAULT false,
    has_children boolean NOT NULL DEFAULT false
);

CREATE UNIQUE INDEX bpm_case_key_idx
    ON bpm_case (tenant, business_key);

CREATE INDEX bpm_case_ready_idx
    ON bpm_case (case_id) WHERE status = 'running' AND has_ready;

CREATE TABLE bpm_case_var (
    case_id   text PRIMARY KEY REFERENCES bpm_case (case_id) ON DELETE CASCADE,
    tenant    text NOT NULL,
    variables jsonb NOT NULL
);

CREATE TABLE bpm_node_outcome (
    case_id    text NOT NULL,
    tenant     text NOT NULL,
    node       text NOT NULL,
    attempt    integer NOT NULL CHECK (attempt >= 1),
    state      text NOT NULL
        CHECK (state IN ('started', 'waiting', 'retrying', 'done', 'failed')),
    output     jsonb,
    branches   jsonb NOT NULL DEFAULT '[]'::jsonb,
    failure    text,
    started_at timestamptz NOT NULL,
    not_before timestamptz,
    is_retry   boolean NOT NULL DEFAULT false,
    due        timestamptz,
    event      text,
    human      text,
    PRIMARY KEY (case_id, node, attempt)
);

CREATE INDEX bpm_node_outcome_event_idx
    ON bpm_node_outcome (tenant, event) WHERE event IS NOT NULL;

CREATE INDEX bpm_node_outcome_due_idx
    ON bpm_node_outcome (due) WHERE due IS NOT NULL;
";

/// The old names, derived from the new ones the way an earlier version of the crate created them:
/// every table, constraint and stand-alone index named `spot_bpm_*` loses its `spot_` prefix.
const DOWNGRADE: &str = r"
DO $$
DECLARE r record;
BEGIN
    FOR r IN SELECT c.relname::text AS name FROM pg_class c
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid
               AND c.relkind = 'r' AND c.relname LIKE 'spot\_bpm\_%' LOOP
        EXECUTE format('ALTER TABLE %I RENAME TO %I', r.name, substr(r.name, 6));
    END LOOP;
    FOR r IN SELECT conrelid::regclass::text AS tbl, conname::text AS name FROM pg_constraint
             WHERE connamespace = to_regnamespace(current_schema())::oid
               AND conname LIKE 'spot\_bpm\_%' LOOP
        EXECUTE format('ALTER TABLE %s RENAME CONSTRAINT %I TO %I', r.tbl, r.name, substr(r.name, 6));
    END LOOP;
    FOR r IN SELECT c.relname::text AS name FROM pg_class c
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid
               AND c.relkind = 'i' AND c.relname LIKE 'spot\_bpm\_%' LOOP
        EXECUTE format('ALTER INDEX %I RENAME TO %I', r.name, substr(r.name, 6));
    END LOOP;
END $$;
";

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

/// Every row of the three tables as canonical JSON text, in a fixed order.
async fn dump(db: &TestDb, prefix: &str) -> Vec<String> {
    let client = db.raw_client().await;
    let mut out = Vec::new();
    for (table, order) in [
        ("bpm_case", "case_id"),
        ("bpm_case_var", "case_id"),
        ("bpm_node_outcome", "case_id, node, attempt"),
    ] {
        let rows = client
            .query(
                &format!("SELECT to_jsonb(t)::text FROM {prefix}{table} t ORDER BY {order}"),
                &[],
            )
            .await
            .unwrap();
        out.extend(
            rows.iter()
                .map(|r| format!("{table}: {}", r.get::<_, String>(0))),
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

/// Two Cases with variables and node outcomes, written through the crate's own API.
async fn seed(store: &PgCaseStore) -> Vec<CaseRecord> {
    let dag = chain(3);
    let mut seeded = Vec::new();
    for (id, key) in [("case_up_a", "up-key-a"), ("case_up_b", "up-key-b")] {
        let record = store.create(new_case(&dag, id, key)).await.unwrap();
        let done = Attempt {
            node: nid("n0"),
            attempt: 1,
            state: AttemptState::Done,
            output: Some(json!({ "value": 7, "who": id })),
            branches: Vec::new(),
            failure: None,
            started_at: Instant::from_offset(now()),
            not_before: None,
            is_retry: false,
        };
        let stored = store
            .commit(
                &CaseId::new(id).unwrap(),
                record.run.version,
                CaseUpdate::new()
                    .with_attempt(done)
                    .with_variables(json!({ "order": { "total": 12, "id": id } })),
            )
            .await
            .unwrap();
        seeded.push(stored);
    }
    seeded
}

/// A database holding the old schema, rows included: the data is written through the crate on
/// the new names, then every name is turned back into the one an earlier version created.
async fn old_database() -> Option<(TestDb, Vec<CaseRecord>, Vec<String>)> {
    let db = TestDb::create().await?;
    let store = PgCaseStore::new(pool(&db, 2, "bpm-up-seed"));
    store.prepare().await.unwrap();
    let seeded = seed(&store).await;
    let new_rows = dump(&db, "spot_").await;
    db.raw_client()
        .await
        .batch_execute(DOWNGRADE)
        .await
        .unwrap();
    Some((db, seeded, new_rows))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_downgrade_helper_matches_the_old_ddl() {
    let Some((old, _, _)) = old_database().await else {
        return;
    };
    let Some(reference) = TestDb::create().await else {
        return;
    };
    reference
        .raw_client()
        .await
        .batch_execute(OLD_DDL)
        .await
        .unwrap();
    assert_eq!(object_names(&old).await, object_names(&reference).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_table_the_crate_creates_is_spot_prefixed_and_passes_the_migrator() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    PgCaseStore::new(pool(&db, 2, "bpm-prefix"))
        .prepare()
        .await
        .unwrap();
    assert_eq!(
        table_names(&db).await,
        vec![
            "spot_bpm_case".to_owned(),
            "spot_bpm_case_var".to_owned(),
            "spot_bpm_node_outcome".to_owned()
        ]
    );
    for name in object_names(&db).await {
        let bare = name.split_once(':').unwrap().1;
        assert!(bare.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
    assert_eq!(migrator_refusals(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_named_tables_are_renamed_with_every_row_kept() {
    let Some((db, seeded, new_rows)) = old_database().await else {
        return;
    };
    let before = dump(&db, "").await;
    assert_eq!(before, new_rows, "the downgrade moves names, not rows");
    assert!(
        migrator_refusals(&db).await > 0,
        "the old names are refused"
    );

    let store = PgCaseStore::new(pool(&db, 2, "bpm-upgrade"));
    store.prepare().await.unwrap();
    // Byte-for-byte the same rows under the new names, the old names gone.
    assert_eq!(dump(&db, "spot_").await, before);
    assert_eq!(table_names(&db).await.len(), 3);
    assert!(
        table_names(&db)
            .await
            .iter()
            .all(|t| t.starts_with("spot_bpm_"))
    );
    // Readable through the crate's own API, with the idempotency guard and key still in force.
    for record in &seeded {
        let loaded = store.load(&record.run.id).await.unwrap().unwrap();
        assert_eq!(&loaded, record);
        let found = store.find_by_key(&record.run.key).await.unwrap().unwrap();
        assert_eq!(found.run.id, record.run.id);
    }
    let dag = chain(3);
    assert!(
        store
            .create(new_case(&dag, "case_up_dup", "up-key-a"))
            .await
            .is_err(),
        "the business-key index survived"
    );
    assert_eq!(migrator_refusals(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgraded_database_has_the_catalogue_of_a_fresh_one() {
    let Some((old, _, _)) = old_database().await else {
        return;
    };
    let Some(fresh) = TestDb::create().await else {
        return;
    };
    PgCaseStore::new(pool(&old, 2, "bpm-cat-old"))
        .prepare()
        .await
        .unwrap();
    PgCaseStore::new(pool(&fresh, 2, "bpm-cat-fresh"))
        .prepare()
        .await
        .unwrap();
    let upgraded = object_names(&old).await;
    assert_eq!(upgraded, object_names(&fresh).await);
    for expected in [
        "con:spot_bpm_case_pkey",
        "con:spot_bpm_case_var_pkey",
        "con:spot_bpm_case_var_case_id_fkey",
        "con:spot_bpm_node_outcome_pkey",
        "idx:spot_bpm_case_key_idx",
        "idx:spot_bpm_case_ready_idx",
        "idx:spot_bpm_node_outcome_event_idx",
        "idx:spot_bpm_node_outcome_due_idx",
    ] {
        assert!(upgraded.iter().any(|n| n == expected), "{expected}");
    }
    assert!(upgraded.iter().all(|n| !n.contains(":bpm_")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_again_changes_nothing() {
    let Some((db, _, _)) = old_database().await else {
        return;
    };
    let store = PgCaseStore::new(pool(&db, 2, "bpm-again"));
    store.prepare().await.unwrap();
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
    let Some((db, _, new_rows)) = old_database().await else {
        return;
    };
    let mut tasks = Vec::new();
    for node in 0..6 {
        let store = PgCaseStore::new(pool(&db, 1, &format!("bpm-node-{node}")));
        tasks.push(tokio::spawn(async move { store.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(dump(&db, "spot_").await, new_rows);
    assert_eq!(table_names(&db).await.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_names_present_is_refused_and_nothing_is_touched() {
    let Some((db, _, _)) = old_database().await else {
        return;
    };
    let client = db.raw_client().await;
    client
        .batch_execute(
            "CREATE TABLE spot_bpm_case_var (LIKE bpm_case_var INCLUDING ALL); \
             INSERT INTO spot_bpm_case_var VALUES ('other', 'tenant', '{\"secret\": 1}')",
        )
        .await
        .unwrap();
    let before = object_names(&db).await;
    let old_rows = dump(&db, "").await;

    let store = PgCaseStore::new(pool(&db, 1, "bpm-both"));
    let error = store.prepare().await.expect_err("no merge by guess");
    let text = error.to_string();
    assert!(
        text.contains("`bpm_case_var`") && text.contains("`spot_bpm_case_var`"),
        "{text}"
    );
    assert!(
        !text.contains("secret") && !text.contains("case_up"),
        "{text}"
    );
    assert_eq!(object_names(&db).await, before);
    assert_eq!(dump(&db, "").await, old_rows);
    let new_count: i64 = client
        .query_one("SELECT count(*) FROM spot_bpm_case_var", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(new_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_table_with_an_old_name_is_not_renamed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client
        .batch_execute(
            "CREATE TABLE bpm_case (case_id text, secret text); \
             INSERT INTO bpm_case VALUES ('x', 'do-not-print')",
        )
        .await
        .unwrap();
    let store = PgCaseStore::new(pool(&db, 1, "bpm-foreign"));
    let error = store
        .prepare()
        .await
        .expect_err("a foreign table is left alone");
    let text = error.to_string();
    assert!(text.contains("`bpm_case`"), "{text}");
    assert!(!text.contains("do-not-print"), "{text}");
    assert_eq!(table_names(&db).await, vec!["bpm_case".to_owned()]);
}
