//! The command ledger over the real engine and PostgreSQL (WP-DOC-04.T8): a kill-and-restart
//! style replay, 50 concurrent calls with one id, a pruned row, a rollback and per-class
//! retention. No secret and no document body is stored in a row's outcome.
//!
//! Needs `SPOT_TEST_PG_URL`; skips (printing why) without it, and fails under
//! `SPOT_REQUIRE_PG=1`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::{meta, n, now};
use spot_app_api::Document;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, SiteId};
use spot_core::ManualClock;
use spot_document::engine::{Engine, RetentionPolicy, V7Ids};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{
    Collation, CommandRecord, Isolation, StorageBackend, Transaction, TxOptions,
};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn policy() -> RetentionPolicy {
    RetentionPolicy::new(
        7 * DAY,
        30 * DAY,
        BTreeMap::from([("bpm".to_owned(), 30 * DAY), ("http_api".to_owned(), DAY)]),
    )
    .unwrap()
}

async fn engine_on(db: &TestDb, policy: RetentionPolicy) -> Engine<PgBackend> {
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

fn actor() -> Actor {
    Actor::User {
        id: RecordId::new_v7(),
        via: None,
    }
}

fn inv(actor: &Actor, command: Option<&str>) -> Invocation {
    Invocation {
        site: SiteId::new("ledger.pg.test").unwrap(),
        actor: actor.clone(),
        command_id: command.map(|c| CommandId::new(c).unwrap()),
        language: None,
        now: now(),
    }
}

fn customer(name: &str) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

async fn row(engine: &Engine<PgBackend>, actor: &Actor, command: &str) -> Option<CommandRecord> {
    let catalog = Arc::new(meta().catalog(Collation::new("und").unwrap()).unwrap());
    let tx = engine
        .backend()
        .begin(
            catalog,
            TxOptions::new(n("probe"), Isolation::ReadCommitted).read_only(),
        )
        .await
        .unwrap();
    let found = tx
        .command_get(&actor.principal_text(), command)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_after_the_commit_and_before_the_reply_replays() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let who = actor();
    let doc = customer("Acme");
    let first = {
        let engine = engine_on(&db, policy()).await;
        let made = engine
            .create(&inv(&who, Some("cmd-1")), doc.clone())
            .await
            .unwrap();
        // The caller never got the reply: the engine and its pool are dropped.
        drop(engine);
        made
    };
    let engine = engine_on(&db, policy()).await;
    let again = engine
        .create(&inv(&who, Some("cmd-1")), doc.clone())
        .await
        .unwrap();
    assert_eq!(first.value.id(), again.value.id());
    assert_eq!(first.value.version(), again.value.version());
    // No document body and no value is in the outcome.
    let stored = row(&engine, &who, "cmd-1").await.unwrap();
    assert!(!stored.outcome.contains("Acme"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_concurrent_calls_with_one_id_commit_once_and_all_answer_the_same_document() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let engine = Arc::new(engine_on(&db, policy()).await);
    let who = actor();
    let doc = customer("Acme");
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let engine = engine.clone();
        let who = who.clone();
        let doc = doc.clone();
        tasks.push(tokio::spawn(async move {
            engine.create(&inv(&who, Some("cmd-1")), doc).await
        }));
    }
    let mut ids = Vec::new();
    for task in tasks {
        let done = task.await.unwrap().unwrap();
        ids.push(done.value.id());
    }
    assert!(ids.iter().all(|id| *id == doc.id()));
    assert!(row(&engine, &who, "cmd-1").await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_operation_leaves_no_row_and_a_pruned_row_no_longer_replays() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let engine = engine_on(&db, policy()).await;
    let who = actor();
    // Validation refuses (`name` is required): rolled back, nothing recorded.
    let bad = Document::new(n("customer"), RecordId::new_v7());
    engine
        .create(&inv(&who, Some("cmd-1")), bad)
        .await
        .unwrap_err();
    assert!(row(&engine, &who, "cmd-1").await.is_none());

    let doc = customer("Acme");
    engine
        .create(&inv(&who, Some("cmd-1")), doc.clone())
        .await
        .unwrap();
    assert!(row(&engine, &who, "cmd-1").await.is_some());
    // Not yet due: still replays.
    assert_eq!(engine.prune_commands(now()).await.unwrap(), 0);
    engine
        .create(&inv(&who, Some("cmd-1")), doc.clone())
        .await
        .unwrap();
    // Due (seven days on): pruned, and the same request is no longer answered as a replay.
    let later = now() + time::Duration::days(8);
    assert_eq!(engine.prune_commands(later).await.unwrap(), 1);
    assert!(row(&engine, &who, "cmd-1").await.is_none());
    engine
        .create(&inv(&who, Some("cmd-1")), doc)
        .await
        .unwrap_err();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_is_per_class_per_row_and_a_policy_change_leaves_old_rows_alone() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let engine = engine_on(&db, policy()).await;
    let who = actor();
    engine
        .for_class("bpm")
        .create(&inv(&who, Some("a")), customer("Acme"))
        .await
        .unwrap();
    engine
        .for_class("http_api")
        .create(&inv(&who, Some("b")), customer("Globex"))
        .await
        .unwrap();
    let a = row(&engine, &who, "a").await.unwrap();
    let b = row(&engine, &who, "b").await.unwrap();
    assert_eq!(a.retain_until - a.recorded_at, time::Duration::days(30));
    assert_eq!(b.retain_until - b.recorded_at, time::Duration::days(1));
    drop(engine);

    // The setting changes: the existing rows keep their dates and still replay.
    let one_day = RetentionPolicy::new(DAY, DAY, BTreeMap::new()).unwrap();
    let rebuilt = engine_on(&db, one_day).await;
    let after = row(&rebuilt, &who, "a").await.unwrap();
    assert_eq!(after.retain_until, a.retain_until);
}
