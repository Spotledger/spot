//! The command store, the lifecycle bounds and the operation lock.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use serde_json::json;
use spot_mgmt_api::lifecycle::{
    CommandRecord, CommandState, CommandStore, LockDenied, MAX_CONCURRENT_COMMANDS,
    MAX_PROGRESS_LINES, MemoryCommandStore, OperationLocks, ProgressLine, StoreError,
};

fn record(id: &str) -> CommandRecord {
    CommandRecord {
        id: id.to_owned(),
        operation: "site.status".to_owned(),
        principal: "alice".to_owned(),
        state: CommandState::Accepted,
        progress: Vec::new(),
        result: None,
        error_code: None,
        audit_ref: String::new(),
        created_at: 100,
        finished_at: None,
        idempotency_key: format!("key-{id}"),
        body_digest: "d".to_owned(),
        cancellable: false,
    }
}

#[tokio::test]
async fn a_duplicate_id_is_refused_and_states_only_move_forward() {
    let store = MemoryCommandStore::new();
    store.insert(record("a"), 100).await.unwrap();
    assert_eq!(
        store.insert(record("a"), 100).await,
        Err(StoreError::Duplicate)
    );
    store.start("a", "audit-1").await.unwrap();
    assert_eq!(
        store.get("a").await.unwrap().unwrap().state,
        CommandState::Running
    );
    store
        .finish("a", CommandState::Succeeded, Some(json!({})), None, 110)
        .await
        .unwrap();
    store
        .finish(
            "a",
            CommandState::Failed,
            None,
            Some("mgmt.internal".into()),
            120,
        )
        .await
        .unwrap();
    let done = store.get("a").await.unwrap().unwrap();
    assert_eq!(done.state, CommandState::Succeeded);
    assert_eq!(done.finished_at, Some(110));
}

#[tokio::test]
async fn progress_lines_are_bounded() {
    let store = MemoryCommandStore::new();
    store.insert(record("a"), 100).await.unwrap();
    for n in 0..100u8 {
        let line = ProgressLine {
            step: format!("s{n}"),
            percent: n,
            at: 100,
        };
        store.progress("a", line).await.unwrap();
    }
    let stored = store.get("a").await.unwrap().unwrap();
    assert_eq!(stored.progress.len(), MAX_PROGRESS_LINES);
    assert_eq!(stored.progress.last().unwrap().step, "s99");
}

#[tokio::test]
async fn finished_commands_expire_but_running_ones_do_not() {
    let store = MemoryCommandStore::with_retention(1000);
    store.insert(record("old"), 100).await.unwrap();
    store.insert(record("live"), 100).await.unwrap();
    store
        .finish("old", CommandState::Succeeded, None, None, 200)
        .await
        .unwrap();
    // A later insert purges what ended more than the retention ago.
    store.insert(record("new"), 5000).await.unwrap();
    assert!(store.get("old").await.unwrap().is_none());
    assert!(store.get("live").await.unwrap().is_some());
    assert_eq!(store.purge(i64::MAX).await.unwrap(), 0);
}

#[tokio::test]
async fn idempotency_lookup_is_per_principal() {
    let store = MemoryCommandStore::new();
    store.insert(record("a"), 100).await.unwrap();
    assert!(
        store
            .find_by_idempotency("alice", "key-a")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .find_by_idempotency("bob", "key-a")
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_lock_allows_four_shared_holders_or_one_exclusive() {
    let locks = OperationLocks::new();
    let held: Vec<_> = (0..MAX_CONCURRENT_COMMANDS)
        .map(|_| locks.try_acquire(false).unwrap())
        .collect();
    assert_eq!(locks.try_acquire(false).unwrap_err(), LockDenied::Saturated);
    assert_eq!(locks.try_acquire(true).unwrap_err(), LockDenied::Conflict);
    drop(held);
    let exclusive = locks.try_acquire(true).unwrap();
    assert_eq!(locks.try_acquire(true).unwrap_err(), LockDenied::Conflict);
    assert_eq!(locks.try_acquire(false).unwrap_err(), LockDenied::Conflict);
    drop(exclusive);
    assert!(locks.try_acquire(false).is_ok());
}
