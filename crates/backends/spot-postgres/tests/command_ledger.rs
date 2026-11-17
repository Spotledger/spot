//! The command ledger table and its three transaction operations against real PostgreSQL
//! (storage contract 0.3.0, WP-DOC-04.T8). Needs `SPOT_TEST_PG_URL`; skips without it and fails
//! under `SPOT_REQUIRE_PG=1`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{catalog, env, options};
use spot_storage_contract::{CommandRecord, ErrorKind, Isolation, StorageBackend, Transaction};
use time::{Duration, OffsetDateTime};

fn at() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
}

fn record(principal: &str, command: &str, retain: OffsetDateTime) -> CommandRecord {
    CommandRecord::new(
        principal,
        command,
        vec![1, 2, 3],
        r#"{"v":1}"#,
        at(),
        retain,
    )
    .unwrap()
}

async fn put(
    env: &common::Env,
    rec: CommandRecord,
) -> Result<(), spot_storage_contract::StorageError> {
    let mut tx = env
        .begin(options("ledger_put", Isolation::ReadCommitted))
        .await
        .unwrap();
    match tx.command_put(rec).await {
        Ok(()) => tx.commit().await,
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

async fn get(env: &common::Env, principal: &str, command: &str) -> Option<CommandRecord> {
    let tx = env
        .begin(options("ledger_get", Isolation::ReadCommitted).read_only())
        .await
        .unwrap();
    let found = tx.command_get(principal, command).await.unwrap();
    tx.rollback().await.unwrap();
    found
}

#[tokio::test]
async fn prepare_creates_the_table_and_is_idempotent() {
    let Some(env) = env().await else { return };
    env.backend.prepare(&catalog()).await.unwrap();
    env.backend.prepare(&catalog()).await.unwrap();
    let raw = env.db.raw_client().await;
    let row = raw
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'spot_command_ledger'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
}

#[tokio::test]
async fn a_record_is_seen_by_its_own_transaction_and_by_others_only_after_commit() {
    let Some(env) = env().await else { return };
    env.backend.prepare(&catalog()).await.unwrap();
    let rec = record("user:a", "cmd-1", at() + Duration::days(7));
    let mut tx = env
        .begin(options("ledger_own", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.command_put(rec.clone()).await.unwrap();
    assert_eq!(
        tx.command_get("user:a", "cmd-1")
            .await
            .unwrap()
            .unwrap()
            .fingerprint,
        vec![1, 2, 3]
    );
    assert!(get(&env, "user:a", "cmd-1").await.is_none());
    tx.commit().await.unwrap();
    let stored = get(&env, "user:a", "cmd-1").await.unwrap();
    assert_eq!(stored.retain_until, rec.retain_until);
    assert_eq!(stored.recorded_at, rec.recorded_at);
    // The outcome is `jsonb`, read back as JSON text.
    assert!(stored.outcome.contains("\"v\""));
}

#[tokio::test]
async fn the_table_rejects_a_second_row_for_one_key() {
    let Some(env) = env().await else { return };
    env.backend.prepare(&catalog()).await.unwrap();
    put(&env, record("user:a", "cmd-1", at() + Duration::days(1)))
        .await
        .unwrap();
    let err = put(&env, record("user:a", "cmd-1", at() + Duration::days(2)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::UniqueViolation);
    // The raw table says the same.
    let raw = env.db.raw_client().await;
    let err = raw
        .execute(
            "INSERT INTO spot_command_ledger VALUES ('user:a', 'cmd-1', '\\x01', '{}', now(), now())",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(err.code().map(|c| c.code()), Some("23505"));
    // Another principal may use the same id.
    put(&env, record("user:b", "cmd-1", at() + Duration::days(1)))
        .await
        .unwrap();
    assert!(get(&env, "user:b", "cmd-1").await.is_some());
}

#[tokio::test]
async fn a_rollback_leaves_no_row() {
    let Some(env) = env().await else { return };
    env.backend.prepare(&catalog()).await.unwrap();
    let mut tx = env
        .begin(options("ledger_rb", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.command_put(record("user:a", "cmd-1", at() + Duration::days(1)))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(get(&env, "user:a", "cmd-1").await.is_none());
}

#[tokio::test]
async fn prune_deletes_by_retain_until_and_the_boundary_is_inclusive() {
    let Some(env) = env().await else { return };
    env.backend.prepare(&catalog()).await.unwrap();
    let cutoff = at() + Duration::days(10);
    // Recorded long ago, its own date after the cut-off: kept.
    let mut kept = record("p", "kept", cutoff + Duration::days(1));
    kept.recorded_at = at() - Duration::days(400);
    put(&env, kept).await.unwrap();
    put(&env, record("p", "gone", cutoff - Duration::hours(1)))
        .await
        .unwrap();
    put(&env, record("p", "edge", cutoff)).await.unwrap();

    let mut tx = env
        .begin(options("ledger_prune", Isolation::ReadCommitted))
        .await
        .unwrap();
    assert_eq!(tx.command_prune(cutoff).await.unwrap(), 2);
    tx.commit().await.unwrap();
    assert!(get(&env, "p", "kept").await.is_some());
    assert!(get(&env, "p", "gone").await.is_none());
    assert!(get(&env, "p", "edge").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_concurrent_first_uses_of_one_key_commit_exactly_once() {
    let Some(env) = env().await else { return };
    env.backend.prepare(&catalog()).await.unwrap();
    let backend = Arc::new(env.backend.clone());
    let mut tasks = Vec::new();
    for i in 0..50u8 {
        let backend = backend.clone();
        tasks.push(tokio::spawn(async move {
            let mut tx = backend
                .begin(catalog(), options("ledger_race", Isolation::ReadCommitted))
                .await
                .unwrap();
            let mut rec = record("user:a", "cmd-1", at() + Duration::days(1));
            rec.fingerprint = vec![i];
            match tx.command_put(rec).await {
                Ok(()) => tx.commit().await,
                Err(e) => {
                    let _ = tx.rollback().await;
                    Err(e)
                }
            }
        }));
    }
    let mut won = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => won += 1,
            Err(e) => assert_eq!(e.kind, ErrorKind::UniqueViolation),
        }
    }
    assert_eq!(won, 1);
    assert!(get(&env, "user:a", "cmd-1").await.is_some());
}
