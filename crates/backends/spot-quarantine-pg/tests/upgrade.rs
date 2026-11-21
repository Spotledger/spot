//! The tables carry the `spot_` prefix `spot_pg_migrate` requires, a database that still holds
//! the old names is upgraded without losing a row, and `prepare` is safe to run again and from
//! several nodes at once.
//!
//! Every test here needs a server and skips cleanly without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use deadpool_postgres::Pool;
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_quarantine::{ItemFilter, QuarantineStore, State};
use spot_quarantine_pg::PgQuarantineStore;
use spot_storage_contract::{Catalog, Collation};

/// The schema as the crate created it before the prefix: the same shape, the old names, the old
/// triggers, plus three items (one closed) with their payloads.
const OLD_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS quarantine_item (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source         text NOT NULL,
    seq            bigint NOT NULL CHECK (seq >= 0),
    record_name    text NOT NULL,
    record_version bigint NOT NULL CHECK (record_version >= 1),
    entity         text NOT NULL,
    record_id      text NOT NULL,
    reason_code    text NOT NULL,
    detail         text NOT NULL,
    first_seen     bigint NOT NULL,
    attempts       integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    state          text NOT NULL
        CHECK (state IN ('held', 'retrying', 'released', 'discarded'))
);

ALTER TABLE quarantine_item ADD COLUMN IF NOT EXISTS closed_by text;
ALTER TABLE quarantine_item ADD COLUMN IF NOT EXISTS closed_at bigint;

CREATE TABLE IF NOT EXISTS quarantine_payload (
    item_id      bigint PRIMARY KEY REFERENCES quarantine_item (id),
    payload      text NOT NULL,
    topic        text,
    conversation text
);

DO $do$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conname = 'quarantine_item_sizes'
                     AND conrelid = 'quarantine_item'::regclass) THEN
        ALTER TABLE quarantine_item ADD CONSTRAINT quarantine_item_sizes CHECK (
            octet_length(source) <= 128
            AND octet_length(reason_code) <= 128
            AND octet_length(detail) <= 4096) NOT VALID;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conname = 'quarantine_item_closure'
                     AND conrelid = 'quarantine_item'::regclass) THEN
        ALTER TABLE quarantine_item ADD CONSTRAINT quarantine_item_closure CHECK (
            (state IN ('held', 'retrying') AND closed_by IS NULL AND closed_at IS NULL)
            OR (state IN ('released', 'discarded')
                AND closed_by IS NOT NULL AND closed_at IS NOT NULL)) NOT VALID;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conname = 'quarantine_payload_size'
                     AND conrelid = 'quarantine_payload'::regclass) THEN
        ALTER TABLE quarantine_payload ADD CONSTRAINT quarantine_payload_size CHECK (
            octet_length(payload) <= 262144) NOT VALID;
    END IF;
END
$do$;

CREATE UNIQUE INDEX IF NOT EXISTS quarantine_item_open_key
    ON quarantine_item (source, record_name, record_version)
    WHERE state IN ('held', 'retrying');

CREATE INDEX IF NOT EXISTS quarantine_item_state_idx ON quarantine_item (state, id);
CREATE INDEX IF NOT EXISTS quarantine_item_reason_idx ON quarantine_item (reason_code, id);
CREATE INDEX IF NOT EXISTS quarantine_item_source_idx ON quarantine_item (source, id);

CREATE OR REPLACE FUNCTION quarantine_refuse_change() RETURNS trigger
LANGUAGE plpgsql AS $fn$
BEGIN
    RAISE EXCEPTION 'quarantine rows are write-once: % of % is refused', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'integrity_constraint_violation';
END;
$fn$;

CREATE OR REPLACE FUNCTION quarantine_item_guard() RETURNS trigger
LANGUAGE plpgsql AS $fn$
BEGIN
    IF OLD.state IN ('released', 'discarded') THEN
        RAISE EXCEPTION 'quarantine item % is closed and cannot change', OLD.id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.source IS DISTINCT FROM OLD.source
       OR NEW.seq IS DISTINCT FROM OLD.seq
       OR NEW.record_name IS DISTINCT FROM OLD.record_name
       OR NEW.record_version IS DISTINCT FROM OLD.record_version
       OR NEW.entity IS DISTINCT FROM OLD.entity
       OR NEW.record_id IS DISTINCT FROM OLD.record_id
       OR NEW.first_seen IS DISTINCT FROM OLD.first_seen THEN
        RAISE EXCEPTION 'the identity of quarantine item % cannot change', OLD.id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END;
$fn$;

DROP TRIGGER IF EXISTS quarantine_item_closed_terminal ON quarantine_item;
CREATE TRIGGER quarantine_item_closed_terminal
    BEFORE UPDATE ON quarantine_item
    FOR EACH ROW EXECUTE FUNCTION quarantine_item_guard();

DROP TRIGGER IF EXISTS quarantine_payload_write_once ON quarantine_payload;
CREATE TRIGGER quarantine_payload_write_once
    BEFORE UPDATE OR DELETE ON quarantine_payload
    FOR EACH ROW EXECUTE FUNCTION quarantine_refuse_change();

DROP TRIGGER IF EXISTS quarantine_payload_no_truncate ON quarantine_payload;
CREATE TRIGGER quarantine_payload_no_truncate
    BEFORE TRUNCATE ON quarantine_payload
    FOR EACH STATEMENT EXECUTE FUNCTION quarantine_refuse_change();

DROP TRIGGER IF EXISTS quarantine_item_keep ON quarantine_item;
CREATE TRIGGER quarantine_item_keep
    BEFORE DELETE ON quarantine_item
    FOR EACH ROW EXECUTE FUNCTION quarantine_refuse_change();

DROP TRIGGER IF EXISTS quarantine_item_no_truncate ON quarantine_item;
CREATE TRIGGER quarantine_item_no_truncate
    BEFORE TRUNCATE ON quarantine_item
    FOR EACH STATEMENT EXECUTE FUNCTION quarantine_refuse_change();
"#;

const OLD_ROWS: &str = r#"
INSERT INTO quarantine_item
    (source, seq, record_name, record_version, entity, record_id, reason_code, detail,
     first_seen, attempts, state, closed_by, closed_at)
VALUES
    ('head.office.app#hub', 1, 'note:a', 1, 'note', 'a', 'unknown_entity', 'secret-detail-1', 100, 2, 'held', NULL, NULL),
    ('head.office.app#hub', 2, 'note:b', 1, 'note', 'b', 'schema_mismatch', 'secret-detail-2', 101, 0, 'discarded', 'admin', 200),
    ('other.site.app#hub', 3, 'note:c', 4, 'note', 'c', 'unknown_entity', 'secret-detail-3', 102, 1, 'retrying', NULL, NULL);
INSERT INTO quarantine_payload (item_id, payload, topic, conversation) VALUES
    (1, '{"image": "payload-one"}', 'orders', NULL),
    (2, '{"image": "payload-two"}', NULL, 'conv-9'),
    (3, '{"image": "payload-three"}', NULL, NULL);
"#;

fn pool(db: &TestDb, size: usize, app: &str) -> Pool {
    common::pool(db, size, app)
}

async fn old_database() -> Option<TestDb> {
    let db = TestDb::create().await?;
    let client = db.raw_client().await;
    client.batch_execute(OLD_DDL).await.unwrap();
    client.batch_execute(OLD_ROWS).await.unwrap();
    Some(db)
}

/// Every relation, constraint, index, sequence and trigger name of the current schema, sorted.
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
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid \
             UNION SELECT 'trg:' || g.tgname::text FROM pg_trigger g \
             JOIN pg_class c ON c.oid = g.tgrelid \
             WHERE NOT g.tgisinternal \
               AND c.relnamespace = to_regnamespace(current_schema())::oid \
             UNION SELECT 'seq:' || relname::text FROM pg_class \
             WHERE relkind = 'S' AND relnamespace = to_regnamespace(current_schema())::oid \
             ORDER BY 1",
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

/// Every row of both tables as text, in a fixed order: compares the stored bytes of the data.
async fn dump(db: &TestDb, item: &str, payload: &str) -> (Vec<String>, Vec<String>) {
    let client = db.raw_client().await;
    let items = client
        .query(&format!("SELECT t::text FROM {item} t ORDER BY id"), &[])
        .await
        .unwrap();
    let payloads = client
        .query(
            &format!("SELECT t::text FROM {payload} t ORDER BY item_id"),
            &[],
        )
        .await
        .unwrap();
    (
        items.iter().map(|r| r.get(0)).collect(),
        payloads.iter().map(|r| r.get(0)).collect(),
    )
}

async fn migrator_refusals(db: &TestDb) -> usize {
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    plan(&live, &wanted).refused.len()
}

fn new_store(db: &TestDb, size: usize, app: &str) -> PgQuarantineStore {
    PgQuarantineStore::new(pool(db, size, app))
}

const NEW_TABLES: [&str; 2] = ["spot_quarantine_item", "spot_quarantine_payload"];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_object_the_crate_creates_is_spot_prefixed_and_passes_the_migrator() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    new_store(&db, 2, "q_prefix").prepare().await.unwrap();
    assert_eq!(table_names(&db).await, NEW_TABLES.map(str::to_owned));
    for name in object_names(&db).await {
        let bare = name.split_once(':').unwrap().1;
        assert!(bare.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
    assert_eq!(migrator_refusals(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_named_tables_are_renamed_with_every_row_kept() {
    let Some(db) = old_database().await else {
        return;
    };
    let before = dump(&db, "quarantine_item", "quarantine_payload").await;
    assert_eq!((before.0.len(), before.1.len()), (3, 3));
    // The upgrade must see the old names, so the migrator refuses them before it ran.
    assert!(migrator_refusals(&db).await > 0);

    let store = new_store(&db, 2, "q_upgrade");
    store.prepare().await.unwrap();
    // Byte-for-byte the same rows under the new names, the old names gone.
    assert_eq!(
        dump(&db, "spot_quarantine_item", "spot_quarantine_payload").await,
        before
    );
    assert_eq!(table_names(&db).await, NEW_TABLES.map(str::to_owned));
    // The crate's own API reads them, payloads included.
    let all = store.list(&ItemFilter::all()).await.unwrap();
    assert_eq!(all.len(), 3);
    let held = store
        .list(&ItemFilter {
            state: Some(State::Held),
            ..ItemFilter::all()
        })
        .await
        .unwrap();
    assert_eq!(held.len(), 1);
    let one = store.item(held[0].id).await.unwrap().expect("item 1");
    assert_eq!(one.reason_code, "unknown_entity");
    let stored: String = db
        .raw_client()
        .await
        .query_one(
            "SELECT payload FROM spot_quarantine_payload WHERE item_id = 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, "{\"image\": \"payload-one\"}");
    // The closed item is still closed (closure intact) and the immutability triggers still fire.
    let client = db.raw_client().await;
    for sql in [
        "UPDATE spot_quarantine_item SET detail = 'x' WHERE id = 2",
        "DELETE FROM spot_quarantine_item WHERE id = 1",
        "UPDATE spot_quarantine_payload SET payload = '{}' WHERE item_id = 1",
        "DELETE FROM spot_quarantine_payload WHERE item_id = 1",
        "TRUNCATE spot_quarantine_payload",
        "TRUNCATE spot_quarantine_item CASCADE",
    ] {
        assert!(client.execute(sql, &[]).await.is_err(), "{sql} was allowed");
    }
    // The identity continues after the highest id (the sequence was renamed, not recreated).
    let id: i64 = client
        .query_one(
            "INSERT INTO spot_quarantine_item \
             (source, seq, record_name, record_version, entity, record_id, reason_code, detail, \
              first_seen, state) \
             VALUES ('z.site.app#hub', 9, 'note:z', 1, 'note', 'z', 'r', 'd', 1, 'held') \
             RETURNING id",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(id, 4);
    // The open-key index is still unique over open items.
    assert!(
        client
            .execute(
                "INSERT INTO spot_quarantine_item \
                 (source, seq, record_name, record_version, entity, record_id, reason_code, \
                  detail, first_seen, state) \
                 VALUES ('z.site.app#hub', 10, 'note:z', 1, 'note', 'z', 'r', 'd', 1, 'held')",
                &[],
            )
            .await
            .is_err()
    );
    assert_eq!(migrator_refusals(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgraded_database_has_the_catalogue_of_a_fresh_one() {
    let Some(old) = old_database().await else {
        return;
    };
    let Some(fresh) = TestDb::create().await else {
        return;
    };
    new_store(&old, 2, "q_cat_old").prepare().await.unwrap();
    new_store(&fresh, 2, "q_cat_fresh").prepare().await.unwrap();
    let upgraded = object_names(&old).await;
    assert_eq!(upgraded, object_names(&fresh).await);
    for expected in [
        "con:spot_quarantine_item_pkey",
        "con:spot_quarantine_item_sizes",
        "con:spot_quarantine_item_closure",
        "con:spot_quarantine_payload_pkey",
        "con:spot_quarantine_payload_item_id_fkey",
        "con:spot_quarantine_payload_size",
        "idx:spot_quarantine_item_open_key",
        "idx:spot_quarantine_item_state_idx",
        "idx:spot_quarantine_item_reason_idx",
        "idx:spot_quarantine_item_source_idx",
        "seq:spot_quarantine_item_id_seq",
        "trg:spot_quarantine_item_closed_terminal",
        "trg:spot_quarantine_item_keep",
        "trg:spot_quarantine_item_no_truncate",
        "trg:spot_quarantine_payload_write_once",
        "trg:spot_quarantine_payload_no_truncate",
    ] {
        assert!(upgraded.iter().any(|n| n == expected), "missing {expected}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_again_changes_nothing() {
    let Some(db) = old_database().await else {
        return;
    };
    let store = new_store(&db, 2, "q_again");
    store.prepare().await.unwrap();
    let names = object_names(&db).await;
    let data = dump(&db, "spot_quarantine_item", "spot_quarantine_payload").await;
    for _ in 0..3 {
        store.prepare().await.unwrap();
    }
    assert_eq!(object_names(&db).await, names);
    assert_eq!(
        dump(&db, "spot_quarantine_item", "spot_quarantine_payload").await,
        data
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_preparing_at_once_upgrade_once_and_lose_nothing() {
    let Some(db) = old_database().await else {
        return;
    };
    let before = dump(&db, "quarantine_item", "quarantine_payload").await;
    let mut tasks = Vec::new();
    for node in 0..6 {
        let store = new_store(&db, 1, &format!("q_node_{node}"));
        tasks.push(tokio::spawn(async move { store.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(
        dump(&db, "spot_quarantine_item", "spot_quarantine_payload").await,
        before
    );
    assert_eq!(table_names(&db).await, NEW_TABLES.map(str::to_owned));
    let fresh = TestDb::create().await.unwrap();
    new_store(&fresh, 1, "q_node_fresh")
        .prepare()
        .await
        .unwrap();
    assert_eq!(object_names(&db).await, object_names(&fresh).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_names_present_is_refused_and_nothing_is_touched() {
    let Some(db) = old_database().await else {
        return;
    };
    let client = db.raw_client().await;
    client
        .batch_execute(
            "CREATE TABLE spot_quarantine_item (LIKE quarantine_item INCLUDING ALL); \
             INSERT INTO spot_quarantine_item OVERRIDING SYSTEM VALUE \
             SELECT * FROM quarantine_item WHERE id = 1",
        )
        .await
        .unwrap();
    let names = object_names(&db).await;
    let old_rows = dump(&db, "quarantine_item", "quarantine_payload").await;
    let new_rows = dump(&db, "spot_quarantine_item", "quarantine_payload")
        .await
        .0;

    let error = new_store(&db, 1, "q_both")
        .prepare()
        .await
        .expect_err("no merge by guess");
    let text = error.to_string();
    assert!(
        text.contains("`quarantine_item`") && text.contains("`spot_quarantine_item`"),
        "{text}"
    );
    assert!(
        !text.contains("secret") && !text.contains("payload-"),
        "{text}"
    );
    assert_eq!(object_names(&db).await, names);
    assert_eq!(
        dump(&db, "quarantine_item", "quarantine_payload").await,
        old_rows
    );
    assert_eq!(
        dump(&db, "spot_quarantine_item", "quarantine_payload")
            .await
            .0,
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
            "CREATE TABLE quarantine_item (id text PRIMARY KEY, secret text); \
             INSERT INTO quarantine_item VALUES ('x', 'do-not-print')",
        )
        .await
        .unwrap();
    let error = new_store(&db, 1, "q_foreign")
        .prepare()
        .await
        .expect_err("a foreign table is left alone");
    let text = error.to_string();
    assert!(text.contains("`quarantine_item`"), "{text}");
    assert!(!text.contains("do-not-print"), "{text}");
    // The refusal rolled everything back: no new table was created either.
    assert_eq!(table_names(&db).await, vec!["quarantine_item".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_taken_target_name_is_refused_before_anything_is_renamed() {
    let Some(db) = old_database().await else {
        return;
    };
    // A foreign relation already holds the new name of an index.
    db.raw_client()
        .await
        .batch_execute("CREATE TABLE spot_quarantine_item_state_idx (x int)")
        .await
        .unwrap();
    let names = object_names(&db).await;
    let error = new_store(&db, 1, "q_taken")
        .prepare()
        .await
        .expect_err("a taken name is refused");
    let text = error.to_string();
    assert!(text.contains("`spot_quarantine_item_state_idx`"), "{text}");
    assert_eq!(object_names(&db).await, names);
    assert_eq!(table_names(&db).await.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_table_of_another_schema_is_never_touched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client
        .batch_execute(&format!(
            "CREATE SCHEMA other; SET search_path TO other; {OLD_DDL} {OLD_ROWS}"
        ))
        .await
        .unwrap();
    new_store(&db, 1, "q_other_schema").prepare().await.unwrap();
    let rows: i64 = client
        .query_one("SELECT count(*) FROM other.quarantine_item", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 3);
    let renamed: bool = client
        .query_one(
            "SELECT to_regclass('other.spot_quarantine_item') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!renamed);
}
