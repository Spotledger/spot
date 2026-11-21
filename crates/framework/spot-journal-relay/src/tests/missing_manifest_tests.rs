//! A missing manifest beside segment objects is a broken chain: `open` refuses and writes nothing.

use spot_journal_manifest::Fork;
use spot_journal_store::JournalStore;

use super::support::*;
use crate::{Durability, Relay};

fn stray_segment(tenant: &str, generation: u64) -> String {
    spot_journal_store::Layout::segment(tenant, generation, 1, 3)
        .unwrap()
        .to_key()
}

async fn listing(store: &FaultStore) -> Vec<String> {
    store.keys_under("").await
}

async fn sealed_generation_one(store: &FaultStore) {
    let mut relay = open(store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    relay.append_batch(batch(1, 1, 6), 2_000).await.unwrap();
    relay.seal_now(3_000).await.unwrap();
    assert_eq!(stored_segments(store, 1).await.len(), 1);
}

#[tokio::test]
async fn segments_present_and_manifest_deleted_is_chain_broken_and_writes_nothing() {
    let store = FaultStore::new();
    sealed_generation_one(&store).await;
    store.delete(&manifest_key(1)).await.unwrap();
    let before = listing(&store).await;
    let segment_before = stored_segments(&store, 1).await;
    store.clear_log();

    let error = open(&store, never_due(Durability::RemoteSync))
        .await
        .err()
        .expect("refused");

    assert_eq!(error.code(), "journal_relay.chain_broken");
    assert!(
        !error.to_string().contains(&segment_prefix(1)),
        "the reason names no object key"
    );
    assert_eq!(
        listing(&store).await,
        before,
        "no new manifest, no deletion"
    );
    assert_eq!(stored_segments(&store, 1).await, segment_before);
    assert!(store.read(&manifest_key(1)).await.is_none());
    assert!(store.log().is_empty(), "no write was attempted");
}

#[tokio::test]
async fn an_empty_prefix_creates_the_first_manifest() {
    let store = FaultStore::new();
    let relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    assert_eq!(relay.generation(), 1);
    assert_eq!(stored_manifest(&store, 1).await.last_seq(), 0);
}

#[tokio::test]
async fn a_tail_chunk_alone_creates_the_first_manifest_and_resume_seals_it() {
    let store = FaultStore::new();
    {
        let mut relay = open(&store, never_due(Durability::RemoteSync))
            .await
            .unwrap();
        relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    }
    assert!(store.keys_under(&segment_prefix(1)).await.is_empty());
    assert_eq!(store.keys_under(&tail_prefix(1)).await.len(), 1);
    store.delete(&manifest_key(1)).await.unwrap();

    let relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();

    assert_eq!(relay.next_position().seq, 4);
    assert!(store.read(&manifest_key(1)).await.is_some());
    assert_eq!(
        entries_of(&stored_segments(&store, 1).await),
        input(1, 1, 3)
    );
}

#[tokio::test]
async fn a_fork_with_segments_under_the_new_generation_and_no_manifest_is_chain_broken() {
    let store = FaultStore::new();
    sealed_generation_one(&store).await;
    store.put(&stray_segment("acme", 2), b"x".to_vec()).await;
    let before = listing(&store).await;

    let error = Relay::open_fork(
        store.clone(),
        Box::new(FixedSigner),
        keys(),
        never_due(Durability::RemoteSync),
        Fork::new(2, 4, "point-in-time rebuild", 7_000),
        8_000,
    )
    .await
    .err()
    .expect("refused");

    assert_eq!(error.code(), "journal_relay.chain_broken");
    assert_eq!(listing(&store).await, before);
    assert!(store.read(&manifest_key(2)).await.is_none());
}

#[tokio::test]
async fn another_tenants_segments_do_not_trigger_the_refusal() {
    let store = FaultStore::new();
    store.put(&stray_segment("other", 1), b"x".to_vec()).await;

    let relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();

    assert_eq!(relay.generation(), 1);
    assert!(store.read(&manifest_key(1)).await.is_some());
}
