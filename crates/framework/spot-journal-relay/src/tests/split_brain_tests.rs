//! A second writer stops the relay, and nothing more is written.

use super::support::*;
use crate::{Durability, RelayError};

async fn assert_halted(relay: &mut crate::Relay<FaultStore>, store: &FaultStore) {
    let writes = store.writes();
    let error = relay
        .append_batch(batch(99, 1, 1), 9_000)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "journal_relay.split_brain");
    assert_eq!(
        relay.seal_now(9_000).await.unwrap_err().code(),
        "journal_relay.split_brain"
    );
    assert_eq!(
        relay.seal_if_due(9_000).await.unwrap_err().code(),
        "journal_relay.split_brain"
    );
    assert_eq!(
        relay.resume().await.unwrap_err().code(),
        "journal_relay.split_brain"
    );
    assert_eq!(store.writes(), writes, "a halted relay writes nothing");
}

#[tokio::test]
async fn a_manifest_changed_by_another_relay_stops_this_one() {
    let store = FaultStore::new();
    let mut first = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    let mut second = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();

    first.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    first.seal_now(3_000).await.unwrap();

    // The second relay never saw that; its seal finds the manifest changed.
    second.append_batch(batch(1, 1, 2), 4_000).await.unwrap();
    let error = second.seal_now(5_000).await.unwrap_err();
    assert!(matches!(error, RelayError::SplitBrain { .. }), "{error:?}");
    assert_halted(&mut second, &store).await;

    // The winner is untouched.
    assert_eq!(stored_manifest(&store, 1).await.last_seq(), 3);
}

#[tokio::test]
async fn a_manifest_replace_that_loses_the_etag_race_stops_the_relay() {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::Local)).await.unwrap();
    relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    store.lose_replace();

    let error = relay.seal_now(3_000).await.unwrap_err();

    assert_eq!(error.code(), "journal_relay.split_brain");
    assert_halted(&mut relay, &store).await;
    assert_eq!(stored_manifest(&store, 1).await.last_seq(), 0);
}

#[tokio::test]
async fn a_tail_chunk_written_by_another_relay_stops_this_one() {
    let store = FaultStore::new();
    let mut first = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    let mut second = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    first.append_batch(batch(1, 1, 3), 2_000).await.unwrap();

    let error = second
        .append_batch(batch(1, 1, 3), 2_000)
        .await
        .unwrap_err();

    assert_eq!(error.code(), "journal_relay.split_brain");
    assert_halted(&mut second, &store).await;
}

#[tokio::test]
async fn a_different_segment_under_the_same_key_stops_the_relay() {
    let store = FaultStore::new();
    let mut first = open(&store, never_due(Durability::Local)).await.unwrap();
    let mut second = open(&store, never_due(Durability::Local)).await.unwrap();
    first.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    first.seal_now(3_000).await.unwrap();
    // Same positions, different content.
    let mut other = batch(1, 1, 3);
    other.entries[0].1 = b"{\"different\":true}".to_vec();
    second.append_batch(other, 2_000).await.unwrap();

    let error = second.seal_now(3_000).await.unwrap_err();

    assert_eq!(error.code(), "journal_relay.split_brain");
    assert_halted(&mut second, &store).await;
}
