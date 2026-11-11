//! The PostgreSQL store on a real database: the conformance suite, the concurrent publish race,
//! crash safety, the size cap and name rules, and `load_into`.
//!
//! Every test skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`; with
//! `SPOT_REQUIRE_PG=1` a missing server fails.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_bpm_definitions::conformance::{run, sample_text};
use spot_bpm_definitions::{DefinitionError, DefinitionStore, MAX_TEXT_BYTES, codes, load_into};
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::ProcessRegistry;
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pg_store_passes_the_conformance_suite() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    run(&store(&db, 4).await, now()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tables_pass_the_migrator_and_prepare_is_repeatable() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2).await;
    store.prepare().await.unwrap();
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    assert_eq!(plan(&live, &wanted).refused.len(), 0);
    let tables = db
        .raw_client()
        .await
        .query(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema() ORDER BY 1",
            &[],
        )
        .await
        .unwrap();
    let tables: Vec<String> = tables.iter().map(|r| r.get(0)).collect();
    assert_eq!(
        tables,
        vec!["spot_bpm_definition", "spot_bpm_definition_head"]
    );
}

/// 16 tasks all believe nothing is published: exactly one wins number 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixteen_concurrent_first_publishes_have_exactly_one_winner() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(store(&db, 16).await);
    let mut tasks = Vec::new();
    for n in 0..16 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let text = sample_text("order", &format!("Doc {n}"));
            store
                .publish("order", &text, None, &format!("user{n}"), None, now())
                .await
        }));
    }
    let mut wins = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(v) => {
                wins += 1;
                assert_eq!(v.version, 1);
            }
            Err(DefinitionError::VersionConflict {
                expected, found, ..
            }) => {
                assert_eq!(expected, None);
                assert_eq!(found, Some(1));
            }
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(store.list("order", None, 100).await.unwrap().len(), 1);
}

/// 16 tasks retry until they win: every number is won once and there are no gaps.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixteen_racing_publishers_get_distinct_numbers_without_gaps() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(store(&db, 16).await);
    let mut tasks = Vec::new();
    for n in 0..16 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let text = sample_text("order", &format!("Doc {n}"));
            loop {
                let latest = store.latest_published("order").await.unwrap();
                let expected = latest.map(|v| v.version);
                match store
                    .publish("order", &text, expected, &format!("user{n}"), None, now())
                    .await
                {
                    Ok(v) => return v.version,
                    Err(DefinitionError::VersionConflict { .. }) => tokio::task::yield_now().await,
                    Err(other) => panic!("unexpected {other:?}"),
                }
            }
        }));
    }
    let mut won = Vec::new();
    for task in tasks {
        won.push(task.await.unwrap());
    }
    won.sort_unstable();
    assert_eq!(won, (1..=16).collect::<Vec<u32>>());
    let stored = store.list("order", None, 100).await.unwrap();
    assert_eq!(
        stored.iter().map(|v| v.version).collect::<Vec<_>>(),
        (1..=16).collect::<Vec<u32>>()
    );
    // Each version holds the text of the publisher that won it.
    for v in stored {
        assert!(v.published_by.starts_with("user"));
        assert!(v.text.contains("Doc "));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_store_on_a_new_pool_sees_everything_the_dropped_one_wrote() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let first = store(&db, 2).await;
    let text = sample_text("order", "Sales Order");
    let v1 = first
        .publish("order", &text, None, "alice", Some("note"), now())
        .await
        .unwrap();
    first.retire("order", 1, "bob", now()).await.unwrap();
    first
        .publish("order", &text, Some(1), "alice", None, now())
        .await
        .unwrap();
    drop(first);

    let second = PgDefinitionStore::new(pool(&db, 2));
    let got = second.get("order", 1).await.unwrap().unwrap();
    assert_eq!(got.text, v1.text);
    assert_eq!(got.content_hash, v1.content_hash);
    assert_eq!(got.retired_by.as_deref(), Some("bob"));
    assert_eq!(
        second
            .latest_published("order")
            .await
            .unwrap()
            .unwrap()
            .version,
        2
    );
    // The counter survived: the next number is 3, and a stale expectation still conflicts.
    assert!(matches!(
        second
            .publish("order", &text, Some(1), "a", None, now())
            .await,
        Err(DefinitionError::VersionConflict { found: Some(2), .. })
    ));
    assert_eq!(
        second
            .publish("order", &text, Some(2), "a", None, now())
            .await
            .unwrap()
            .version,
        3
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_database_itself_holds_the_size_cap_and_the_name_grammar() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2).await;
    // The store refuses an over-size text before it reaches the database...
    let big = "x".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(
        store
            .publish("order", &big, None, "a", None, now())
            .await
            .unwrap_err()
            .code(),
        codes::TOO_LARGE
    );
    // ...and a statement that bypasses the store is refused by the table.
    let raw = db.raw_client().await;
    let insert = "INSERT INTO spot_bpm_definition \
        (tenant, name, version, text, content_hash, state, published_by, published_at) \
        VALUES ('default', $1, 1, $2, $3, 'published', 'x', now())";
    let hash = "a".repeat(64);
    let err = raw
        .execute(insert, &[&"order", &big, &hash])
        .await
        .unwrap_err();
    assert_eq!(err.code().unwrap().code(), "23514");
    for bad in ["../x", "Order", "a b", "a'b", ""] {
        let err = raw
            .execute(insert, &[&bad, &"ok", &hash])
            .await
            .unwrap_err();
        assert_eq!(err.code().unwrap().code(), "23514", "{bad}");
    }
    // A text exactly at the cap is accepted by the database too.
    let at_cap = "x".repeat(MAX_TEXT_BYTES);
    raw.execute(insert, &[&"order", &at_cap, &hash])
        .await
        .unwrap();
    // A name with path characters is refused by the store on every method.
    for bad in ["../etc/passwd", "a/b", "a\\b", "a.b", "A", "é"] {
        assert_eq!(
            store.get(bad, 1).await.unwrap_err().code(),
            codes::NAME_INVALID
        );
        assert_eq!(
            store
                .publish(bad, &sample_text("order", "X"), None, "a", None, now())
                .await
                .unwrap_err()
                .code(),
            codes::NAME_INVALID
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_into_rebuilds_a_registry_from_the_database_after_a_restart() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let first = store(&db, 2).await;
    let text = sample_text("order", "Sales Order");
    let v1 = first
        .publish("order", &text, None, "alice", None, now())
        .await
        .unwrap();
    first
        .publish(
            "invoice",
            &sample_text("invoice", "Sales Invoice"),
            None,
            "alice",
            None,
            now(),
        )
        .await
        .unwrap();
    drop(first);

    let restarted = PgDefinitionStore::new(pool(&db, 2));
    let registry = ProcessRegistry::new();
    let report = load_into(&registry, &restarted).await.unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.loaded.len(), 2);
    let loaded = registry.get("order", 1).unwrap();
    assert_eq!(loaded.compiled.hash.as_hex(), v1.content_hash);
}
