//! `Engine::create_batch` over PostgreSQL: the savepoint per document, the ledger and the
//! commit on the real backend, and the batch against one-by-one creates for the same documents.
//!
//! Needs `SPOT_TEST_PG_URL`; skips (printing why) without it, and fails under
//! `SPOT_REQUIRE_PG=1`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{meta, n, now};
use spot_app_api::Document;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, SiteId};
use spot_core::ManualClock;
use spot_document::engine::{Engine, RetentionPolicy, V7Ids};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::RegistryBuilder;

async fn engine_on(db: &TestDb) -> Engine<PgBackend> {
    let policy = RetentionPolicy::new(
        Duration::from_secs(86_400),
        Duration::from_secs(86_400 * 30),
        BTreeMap::new(),
    )
    .unwrap();
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let engine = Engine::new(
        backend,
        meta(),
        Arc::new(RegistryBuilder::new().build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_command_ledger(policy);
    engine.prepare().await.unwrap();
    engine
}

fn inv(actor: &Actor, command: &str) -> Invocation {
    Invocation {
        site: SiteId::new("batch.pg.test").unwrap(),
        actor: actor.clone(),
        command_id: Some(CommandId::new(command).unwrap()),
        language: None,
        now: now(),
    }
}

fn customer(name: &str) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_commits_every_good_document_and_only_those() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let engine = engine_on(&db).await;
    let who = Actor::User {
        id: RecordId::new_v7(),
        via: None,
    };
    let mut nameless = Document::new(n("customer"), RecordId::new_v7());
    nameless.set(n("note"), "x");
    let mut items: Vec<_> = (0..40)
        .map(|k| (inv(&who, &format!("pg-{k}")), customer(&format!("C{k}"))))
        .collect();
    items.insert(7, (inv(&who, "pg-bad"), nameless.clone()));
    let results = engine.create_batch(items.clone()).await;
    assert_eq!(results.len(), 41);
    assert!(results[7].is_err());
    for (index, ((_, sent), result)) in items.iter().zip(&results).enumerate() {
        if index == 7 {
            continue;
        }
        assert_eq!(result.as_ref().unwrap().value.id(), sent.id());
        assert!(
            engine
                .load(&inv(&who, "read"), "customer", sent.id())
                .await
                .is_ok()
        );
    }
    assert!(
        engine
            .load(&inv(&who, "read"), "customer", nameless.id())
            .await
            .is_err()
    );
    // The same command ids again: replays, nothing new.
    let again = engine.create_batch(items.clone()).await;
    assert!(again[7].is_err());
    assert_eq!(again.iter().filter(|r| r.is_ok()).count(), 40);
}

/// Prints the cost of the batch against one create per document (not an assertion on time).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_costs_less_than_one_create_per_document() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let engine = engine_on(&db).await;
    let who = Actor::User {
        id: RecordId::new_v7(),
        via: None,
    };
    let rows = 300;
    let single = Instant::now();
    for k in 0..rows {
        engine
            .create(&inv(&who, &format!("s-{k}")), customer(&format!("S{k}")))
            .await
            .unwrap();
    }
    let single = single.elapsed();
    let batched = Instant::now();
    for chunk in 0..rows / 100 {
        let items = (0..100)
            .map(|k| {
                (
                    inv(&who, &format!("b-{chunk}-{k}")),
                    customer(&format!("B{chunk}-{k}")),
                )
            })
            .collect();
        for result in engine.create_batch(items).await {
            result.unwrap();
        }
    }
    let batched = batched.elapsed();
    eprintln!("create_batch: {rows} rows one by one {single:?}, in batches of 100 {batched:?}");
    assert!(batched < single, "{batched:?} is not less than {single:?}");
}
