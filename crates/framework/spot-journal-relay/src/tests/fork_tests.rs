//! A fork opens the next generation through its own constructor.

use spot_journal_manifest::Fork;
use spot_journal_segment::SegmentReader;

use super::support::*;
use crate::{Durability, Relay};

async fn open_fork(store: &FaultStore, fork: Fork) -> Result<Relay<FaultStore>, crate::RelayError> {
    Relay::open_fork(
        store.clone(),
        Box::new(FixedSigner),
        keys(),
        never_due(Durability::RemoteSync),
        fork,
        8_000,
    )
    .await
}

async fn first_generation(store: &FaultStore) {
    let mut relay = open(store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    relay.append_batch(batch(1, 1, 6), 2_000).await.unwrap();
    relay.seal_now(3_000).await.unwrap();
}

#[tokio::test]
async fn a_fork_opens_a_new_generation_and_the_old_one_stays_verifiable() {
    let store = FaultStore::new();
    first_generation(&store).await;
    let before = store.read(&manifest_key(1)).await.unwrap();

    let mut relay = open_fork(&store, Fork::new(2, 4, "point-in-time rebuild", 7_000))
        .await
        .unwrap();

    assert_eq!(relay.generation(), 2);
    assert_eq!(relay.next_position().generation, 2);
    assert_eq!(relay.next_position().seq, 1);
    let second = stored_manifest(&store, 2).await;
    assert_eq!(second.forks().len(), 1);
    assert_eq!(second.forks()[0].forked_from_position, 4);
    assert_eq!(
        second.manifest().previous_manifest_hash,
        Some(stored_manifest(&store, 1).await.manifest().manifest_hash())
    );

    // The relay writes only into its own generation.
    relay
        .append_batch(batch_in(2, 1, 1, 3), 8_500)
        .await
        .unwrap();
    relay.seal_now(9_000).await.unwrap();
    assert_eq!(
        relay
            .append_batch(batch(2, 4, 4), 9_500)
            .await
            .unwrap_err()
            .code(),
        "journal_relay.entry_invalid",
        "a generation 1 position is refused"
    );
    let segments = stored_segments(&store, 2).await;
    assert_eq!(segments.len(), 1);
    let reader = SegmentReader::open(segments[0].as_slice()).unwrap();
    assert_eq!(reader.header().generation, 2);
    assert_eq!(entries_of(&segments), input(2, 1, 3));
    assert_eq!(stored_manifest(&store, 2).await.last_seq(), 3);

    // The abandoned generation is untouched, byte for byte.
    assert_eq!(store.read(&manifest_key(1)).await.unwrap(), before);
    assert_eq!(stored_segments(&store, 1).await.len(), 1);
}

#[tokio::test]
async fn reopening_the_forked_generation_needs_the_same_fork_record() {
    let store = FaultStore::new();
    first_generation(&store).await;
    let fork = Fork::new(2, 4, "rebuild", 7_000);
    open_fork(&store, fork.clone()).await.unwrap();

    // The same record reopens it.
    open_fork(&store, fork).await.unwrap();
    // A different record is refused.
    let error = open_fork(&store, Fork::new(2, 5, "rebuild", 7_000))
        .await
        .err()
        .expect("refused");
    assert_eq!(error.code(), "journal_relay.chain_broken");
}

#[tokio::test]
async fn a_closed_generation_cannot_be_written_again() {
    let store = FaultStore::new();
    first_generation(&store).await;
    open_fork(&store, Fork::new(2, 4, "rebuild", 7_000))
        .await
        .unwrap();

    let error = open(&store, never_due(Durability::RemoteSync))
        .await
        .err()
        .expect("refused");

    assert_eq!(error.code(), "journal_relay.chain_broken");
}

#[tokio::test]
async fn a_fork_needs_the_abandoned_generation_and_a_listed_position() {
    let empty = FaultStore::new();
    let error = open_fork(&empty, Fork::new(2, 4, "rebuild", 7_000))
        .await
        .err()
        .expect("refused");
    assert_eq!(error.code(), "journal_relay.chain_broken");

    let store = FaultStore::new();
    first_generation(&store).await;
    let error = open_fork(&store, Fork::new(2, 99, "rebuild", 7_000))
        .await
        .err()
        .expect("refused");
    assert_eq!(error.code(), "journal_relay.chain_broken");
}
