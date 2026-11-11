//! The crash matrix: a store that stops (or loses the answer) after each write of a seal, then
//! `resume` on the same relay and on a fresh one. No entry may be lost or repeated.

use spot_journal_segment::{SealLimits, verify_chain};

use super::support::*;
use crate::{Durability, Relay};

/// Writes of a remote-sync seal of two chunks: segment, manifest, two deletes.
const REMOTE_SEAL_WRITES: usize = 4;
/// Writes of a local seal: segment, manifest.
const LOCAL_SEAL_WRITES: usize = 2;

async fn assert_whole(store: &FaultStore, generation: u64, last: u64) {
    let segments = stored_segments(store, generation).await;
    assert_eq!(
        entries_of(&segments),
        input(generation, 1, last),
        "no entry lost, none repeated"
    );
    verify_chain(&segments).expect("the chain verifies");
    let manifest = stored_manifest(store, generation).await;
    assert_eq!(manifest.last_seq(), last);
    assert_eq!(manifest.segments().len(), segments.len());
    assert!(store.keys_under(&tail_prefix(generation)).await.is_empty());
}

#[tokio::test]
async fn remote_sync_seal_survives_a_stop_after_every_write() {
    for k in 0..REMOTE_SEAL_WRITES {
        for apply in [false, true] {
            for crash in [false, true] {
                let label = format!("k={k} apply={apply} crash={crash}");
                let store = FaultStore::new();
                let mut relay = open(&store, never_due(Durability::RemoteSync))
                    .await
                    .unwrap();
                relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
                relay.append_batch(batch(2, 4, 6), 2_000).await.unwrap();

                store.fail_after(k, apply);
                let error = relay.seal_now(3_000).await.unwrap_err();
                assert_eq!(error.code(), "journal_relay.store_failed", "{label}");
                // The relay knows it is unsure.
                assert_eq!(
                    relay.append_batch(batch(3, 7, 9), 3_000).await.unwrap_err(),
                    crate::RelayError::ResumeRequired,
                    "{label}"
                );
                store.heal();

                let mut relay = if crash {
                    drop(relay);
                    open(&store, never_due(Durability::RemoteSync))
                        .await
                        .unwrap()
                } else {
                    relay.resume().await.unwrap();
                    relay
                };

                assert_whole(&store, 1, 6).await;
                assert_eq!(relay.next_position(), pos(7), "{label}");
                // And the relay carries on.
                relay.append_batch(batch(3, 7, 9), 4_000).await.unwrap();
                relay.seal_now(5_000).await.unwrap();
                assert_whole(&store, 1, 9).await;
            }
        }
    }
}

#[tokio::test]
async fn local_seal_survives_a_stop_after_every_write() {
    for k in 0..LOCAL_SEAL_WRITES {
        for apply in [false, true] {
            for crash in [false, true] {
                let label = format!("k={k} apply={apply} crash={crash}");
                let store = FaultStore::new();
                let mut relay = open(&store, never_due(Durability::Local)).await.unwrap();
                relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
                relay.append_batch(batch(2, 4, 6), 2_000).await.unwrap();

                store.fail_after(k, apply);
                let error = relay.seal_now(3_000).await.unwrap_err();
                assert_eq!(error.code(), "journal_relay.store_failed", "{label}");
                store.heal();

                let mut relay = if crash {
                    drop(relay);
                    open(&store, never_due(Durability::Local)).await.unwrap()
                } else {
                    relay.resume().await.unwrap();
                    relay
                };

                if crash {
                    // The process lost what was only in memory: the caller replays from the
                    // relay's next position.
                    let next = relay.next_position().seq;
                    if next <= 6 {
                        relay.append_batch(batch(10, next, 6), 4_000).await.unwrap();
                    }
                }
                assert_eq!(relay.next_position(), pos(7), "{label}");
                relay.append_batch(batch(11, 7, 9), 4_000).await.unwrap();
                relay.seal_now(5_000).await.unwrap();
                assert_whole(&store, 1, 9).await;
            }
        }
    }
}

#[tokio::test]
async fn a_seal_started_by_an_append_that_fails_leaves_the_new_batch_unwritten() {
    for k in 0..REMOTE_SEAL_WRITES {
        let store = FaultStore::new();
        let mut config = cfg(Durability::RemoteSync);
        config.limits = SealLimits {
            max_bytes: 1,
            max_age_millis: u64::MAX,
        };
        let mut relay = open(&store, config.clone()).await.unwrap();
        relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
        // This one is due to seal the first; the seal has two chunks' worth of writes at most.
        store.fail_after(k.min(2), false);
        let outcome = relay.append_batch(batch(2, 4, 6), 3_000).await;
        let error = outcome.unwrap_err();
        assert_eq!(error.code(), "journal_relay.store_failed");
        assert!(
            store.read(&tail_key(1, 2)).await.is_none(),
            "the new batch was not written before the seal finished"
        );
        store.heal();
        relay.resume().await.unwrap();
        assert_eq!(relay.next_position(), pos(4));
        relay.append_batch(batch(2, 4, 6), 4_000).await.unwrap();
        relay.seal_now(5_000).await.unwrap();
        assert_whole(&store, 1, 6).await;
    }
}

#[tokio::test]
async fn resume_is_idempotent_and_seals_the_chunks_it_finds() {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    // A healthy relay with an unsealed chunk: resume re-seals it into a segment.
    relay.resume().await.unwrap();
    assert_whole(&store, 1, 3).await;
    let writes = store.writes();
    relay.resume().await.unwrap();
    relay.resume().await.unwrap();
    assert_eq!(
        store.writes(),
        writes,
        "a second resume has nothing to repair"
    );
    assert_whole(&store, 1, 3).await;
}

#[tokio::test]
async fn resume_adopts_a_segment_the_manifest_lacks_and_keeps_the_chain() {
    // A crash after step (a): the segment object is stored, the manifest does not list it.
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    store.fail_after(1, false);
    relay.seal_now(3_000).await.unwrap_err();
    store.heal();
    assert_eq!(stored_manifest(&store, 1).await.last_seq(), 0);
    assert_eq!(stored_segments(&store, 1).await.len(), 1);
    drop(relay);

    let relay: Relay<FaultStore> = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();

    assert_eq!(relay.manifest().last_seq(), 3);
    assert_whole(&store, 1, 3).await;
}
