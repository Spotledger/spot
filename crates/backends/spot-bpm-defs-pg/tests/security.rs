//! The security properties named in the brief, each proved on a real database: cross-tenant
//! access, injection, fail-open, immutability, and that a refused write leaves no partial row.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bpm_definitions::conformance::sample_text;
use spot_bpm_definitions::{DefinitionError, DefinitionStore, codes};
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_postgres::testing::TestDb;

fn tenant_store(db: &TestDb, tenant: &str) -> PgDefinitionStore {
    PgDefinitionStore::new(pool(db, 2)).with_tenant(tenant)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_tenant_never_sees_or_changes_another_tenants_definitions() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let acme = tenant_store(&db, "acme");
    acme.prepare().await.unwrap();
    let globex = tenant_store(&db, "globex");
    let text = sample_text("order", "Acme Secret Order");
    acme.publish("order", &text, None, "alice", Some("acme note"), now())
        .await
        .unwrap();
    acme.publish(
        "payroll",
        &sample_text("payroll", "P"),
        None,
        "alice",
        None,
        now(),
    )
    .await
    .unwrap();

    // Every method of the other tenant's store returns nothing of acme's.
    assert!(globex.get("order", 1).await.unwrap().is_none());
    assert!(globex.latest_published("order").await.unwrap().is_none());
    assert!(globex.list("order", None, 100).await.unwrap().is_empty());
    assert!(globex.names().await.unwrap().is_empty());
    assert!(globex.running_pins("order").await.unwrap().is_empty());
    assert_eq!(
        globex
            .retire("order", 1, "mallory", now())
            .await
            .unwrap_err()
            .code(),
        codes::NOT_FOUND
    );
    // The retire attempt changed nothing for acme.
    let kept = acme.get("order", 1).await.unwrap().unwrap();
    assert_eq!(kept.state, spot_bpm_definitions::VersionState::Published);

    // The same name is a separate sequence in each tenant, and a publish expecting acme's latest
    // conflicts in globex.
    assert!(matches!(
        globex
            .publish("order", &text, Some(1), "bob", None, now())
            .await,
        Err(DefinitionError::VersionConflict { found: None, .. })
    ));
    let g1 = globex
        .publish(
            "order",
            &sample_text("order", "Globex"),
            None,
            "bob",
            None,
            now(),
        )
        .await
        .unwrap();
    assert_eq!(g1.version, 1);
    assert_eq!(acme.list("order", None, 100).await.unwrap().len(), 1);
    assert_eq!(acme.names().await.unwrap(), vec!["order", "payroll"]);
    assert_eq!(globex.names().await.unwrap(), vec!["order"]);
    // Acme's text is still acme's.
    assert!(
        acme.get("order", 1)
            .await
            .unwrap()
            .unwrap()
            .text
            .contains("Acme Secret")
    );
    assert!(g1.text.contains("Globex"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_pins_are_the_tenants_running_cases_only() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let acme = tenant_store(&db, "acme");
    acme.prepare().await.unwrap();
    // Before any Case table exists there is no Case, so no pin.
    assert!(acme.running_pins("order").await.unwrap().is_empty());
    let raw = db.raw_client().await;
    raw.batch_execute(spot_bpm_pg::schema::DDL).await.unwrap();
    let insert = "INSERT INTO spot_bpm_case \
        (case_id, tenant, business_key, dag_key, dag_version, content_hash, status, version, \
         started_at, nodes) \
        VALUES ($1, $2, $1, $3, $4, $5, $6, 0, now(), '{}'::jsonb)";
    let hash = "b".repeat(64);
    for (id, tenant, name, version, status) in [
        ("c1", "acme", "order", 1_i32, "running"),
        ("c2", "acme", "order", 1, "running"),
        ("c3", "acme", "order", 3, "running"),
        ("c4", "acme", "order", 2, "completed"),
        ("c5", "acme", "invoice", 7, "running"),
        ("c6", "globex", "order", 9, "running"),
    ] {
        raw.execute(insert, &[&id, &tenant, &name, &version, &hash, &status])
            .await
            .unwrap();
    }
    assert_eq!(acme.running_pins("order").await.unwrap(), vec![1, 3]);
    assert_eq!(acme.running_pins("invoice").await.unwrap(), vec![7]);
    let globex = tenant_store(&db, "globex");
    assert_eq!(globex.running_pins("order").await.unwrap(), vec![9]);
    assert!(globex.running_pins("invoice").await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_values_are_data_and_never_sql() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2).await;
    let hostile = "'); DROP TABLE spot_bpm_definition; --";
    let v = store
        .publish(
            "order",
            &sample_text("order", hostile),
            None,
            hostile,
            Some(hostile),
            now(),
        )
        .await
        .unwrap();
    assert_eq!(v.note.as_deref(), Some(hostile));
    let got = store.get("order", 1).await.unwrap().unwrap();
    assert_eq!(got.published_by, hostile);
    assert!(got.text.contains(hostile));
    // A hostile tenant is a value too: it names an empty tenant, not another one.
    let odd = PgDefinitionStore::new(pool(&db, 2)).with_tenant("default' OR '1'='1");
    assert!(odd.names().await.unwrap().is_empty());
    assert!(odd.get("order", 1).await.unwrap().is_none());
    // Hostile names never reach the database.
    for name in [hostile, "order'; --", "order\" --", "ord er"] {
        assert_eq!(
            store.get(name, 1).await.unwrap_err().code(),
            codes::NAME_INVALID
        );
    }
    assert_eq!(store.names().await.unwrap(), vec!["order"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stored_version_cannot_be_rewritten_or_deleted_even_by_raw_sql() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2).await;
    let text = sample_text("order", "Sales Order");
    store
        .publish("order", &text, None, "alice", Some("n"), now())
        .await
        .unwrap();
    let raw = db.raw_client().await;
    for (sql, what) in [
        ("UPDATE spot_bpm_definition SET text = 'changed'", "text"),
        (
            "UPDATE spot_bpm_definition SET content_hash = repeat('c', 64)",
            "hash",
        ),
        (
            "UPDATE spot_bpm_definition SET published_by = 'mallory'",
            "publisher",
        ),
        ("UPDATE spot_bpm_definition SET note = 'changed'", "note"),
        (
            "UPDATE spot_bpm_definition SET published_at = now()",
            "time",
        ),
        ("UPDATE spot_bpm_definition SET version = 2", "version"),
        ("UPDATE spot_bpm_definition SET name = 'other'", "name"),
        ("UPDATE spot_bpm_definition SET tenant = 'other'", "tenant"),
        ("DELETE FROM spot_bpm_definition", "delete"),
    ] {
        let err = raw.execute(sql, &[]).await.unwrap_err();
        assert_eq!(
            err.code().unwrap().code(),
            "23000",
            "{what} was not refused"
        );
    }
    let kept = store.get("order", 1).await.unwrap().unwrap();
    assert_eq!(kept.text, text);
    assert_eq!(kept.published_by, "alice");

    // Retiring is the one allowed move; undoing it is not.
    store.retire("order", 1, "bob", now()).await.unwrap();
    for sql in [
        "UPDATE spot_bpm_definition SET state = 'published', retired_at = NULL, retired_by = NULL",
        "UPDATE spot_bpm_definition SET retired_by = 'mallory'",
        "UPDATE spot_bpm_definition SET retired_at = now() + interval '1 day'",
    ] {
        let err = raw.execute(sql, &[]).await.unwrap_err();
        assert!(
            matches!(err.code().map(|c| c.code()), Some("23000" | "23514")),
            "{sql}"
        );
    }
    let still = store.get("order", 1).await.unwrap().unwrap();
    assert_eq!(still.retired_by.as_deref(), Some("bob"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_failure_is_an_error_and_never_an_empty_result() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2).await;
    store
        .publish(
            "order",
            &sample_text("order", "S"),
            None,
            "alice",
            None,
            now(),
        )
        .await
        .unwrap();
    // The table is gone under the store: every method fails instead of answering empty.
    let raw = db.raw_client().await;
    raw.batch_execute("DROP TABLE spot_bpm_definition")
        .await
        .unwrap();
    let t = now();
    let results = [
        store.get("order", 1).await.map(|_| ()),
        store.latest_published("order").await.map(|_| ()),
        store.list("order", None, 10).await.map(|_| ()),
        store.names().await.map(|_| ()),
        store.retire("order", 1, "a", t).await.map(|_| ()),
        store
            .publish("order", &sample_text("order", "S"), Some(1), "a", None, t)
            .await
            .map(|_| ()),
    ];
    for result in results {
        let error = result.unwrap_err();
        assert_eq!(error.code(), codes::STORE_FAILED);
        let shown = error.to_string();
        assert!(shown.contains("pg"), "{shown}");
        assert!(
            !shown.contains("order") && !shown.contains("spot_bpm"),
            "{shown}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_pool_is_a_store_failure() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2);
    let store = PgDefinitionStore::new(pool.clone());
    store.prepare().await.unwrap();
    pool.close();
    for error in [
        store.names().await.unwrap_err(),
        store.get("order", 1).await.unwrap_err(),
        store.running_pins("order").await.unwrap_err(),
    ] {
        assert_eq!(error.code(), codes::STORE_FAILED);
    }
    assert_eq!(
        store.prepare().await.unwrap_err().code(),
        codes::STORE_FAILED
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_row_is_an_error_not_skipped_and_leaks_no_content() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2).await;
    let text = sample_text("order", "Sales Order");
    for expected in [None, Some(1)] {
        store
            .publish(
                "order",
                &text,
                expected,
                "alice",
                Some("SECRET-NOTE"),
                now(),
            )
            .await
            .unwrap();
    }
    let raw = db.raw_client().await;
    // Break the state constraint the way a damaged table would be, then corrupt one row.
    raw.batch_execute(
        "ALTER TABLE spot_bpm_definition DROP CONSTRAINT spot_bpm_definition_state_check; \
         UPDATE spot_bpm_definition SET state = 'bogus' WHERE version = 2",
    )
    .await
    .unwrap();
    for error in [
        store.get("order", 2).await.unwrap_err(),
        store.list("order", None, 10).await.unwrap_err(),
    ] {
        assert_eq!(error.code(), codes::STORE_FAILED);
        let shown = error.to_string();
        assert!(
            !shown.contains("SECRET-NOTE") && !shown.contains("alice"),
            "{shown}"
        );
    }
    // `latest_published` filters on the state, so it never reads the damaged row.
    assert_eq!(
        store
            .latest_published("order")
            .await
            .unwrap()
            .unwrap()
            .version,
        1
    );
    // The healthy row is still readable by itself.
    assert_eq!(store.get("order", 1).await.unwrap().unwrap().version, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_write_leaves_no_partial_row_and_the_audit_fields_are_kept() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2).await;
    let raw = db.raw_client().await;
    async fn counts(raw: &tokio_postgres::Client) -> (i64, i64) {
        let rows = raw.query_one("SELECT (SELECT count(*) FROM spot_bpm_definition), (SELECT count(*) FROM spot_bpm_definition_head)", &[]).await.unwrap();
        (rows.get(0), rows.get(1))
    }
    // A conflicting first publish creates neither a version nor a head row.
    let text = sample_text("order", "S");
    assert!(
        store
            .publish("order", &text, Some(4), "a", None, now())
            .await
            .is_err()
    );
    assert!(
        store
            .publish("order", &text, Some(0), "a", None, now())
            .await
            .is_err()
    );
    assert!(
        store
            .publish("order", "garbage", None, "a", None, now())
            .await
            .is_err()
    );
    assert_eq!(counts(&raw).await, (0, 0));

    // After a real publish, refused ones change neither table.
    let v = store
        .publish("order", &text, None, "alice", Some("why"), now())
        .await
        .unwrap();
    assert!(
        store
            .publish("order", &text, None, "b", None, now())
            .await
            .is_err()
    );
    assert!(
        store
            .publish("order", &text, Some(9), "b", None, now())
            .await
            .is_err()
    );
    assert_eq!(counts(&raw).await, (1, 1));
    let head: i32 = raw
        .query_one("SELECT latest FROM spot_bpm_definition_head", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(head, 1);
    // The row keeps who, when and which version.
    assert_eq!(
        (v.published_by.as_str(), v.published_at, v.version),
        ("alice", now(), 1)
    );
    assert_eq!(store.get("order", 1).await.unwrap().unwrap(), v);
}
