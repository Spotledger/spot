//! Batch intake: order, all-or-nothing, the chunk-before-ack rule and sealing.

use spot_journal_segment::{SealLimits, verify_chain};

use super::support::*;
use crate::{Appended, Batch, Durability, RelayError};

#[tokio::test]
async fn remote_sync_writes_the_chunk_before_it_returns_the_ack_value() {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    store.clear_log();

    let appended = relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();

    assert_eq!(
        appended,
        Appended::ChunkDurable {
            batch_id: 1,
            up_to_seq: 3
        }
    );
    // The chunk is already in the store when the call returns, and it was the only write.
    assert_eq!(store.log(), vec![format!("put_new {}", tail_key(1, 1))]);
    assert!(store.read(&tail_key(1, 1)).await.is_some());
    assert_eq!(relay.next_position(), pos(4));
}

#[tokio::test]
async fn a_failed_chunk_write_returns_an_error_not_the_ack_value() {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    store.fail_after(0, false);

    let error = relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap_err();

    assert_eq!(error.code(), "journal_relay.store_failed");
    // Until it is resumed the relay takes nothing.
    store.heal();
    let error = relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap_err();
    assert_eq!(error, RelayError::ResumeRequired);
    relay.resume().await.unwrap();
    relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
}

#[tokio::test]
async fn local_and_replica_sync_write_no_chunk() {
    for durability in [Durability::Local, Durability::ReplicaSync] {
        let store = FaultStore::new();
        let mut relay = open(&store, never_due(durability)).await.unwrap();
        let writes = store.writes();

        let appended = relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();

        assert_eq!(
            appended,
            Appended::Buffered {
                batch_id: 1,
                up_to_seq: 3
            }
        );
        assert_eq!(store.writes(), writes, "nothing is written until a seal");
        assert!(store.keys_under(&tail_prefix(1)).await.is_empty());
        assert_eq!(relay.open_entries(), 3);
    }
}

#[tokio::test]
async fn positions_must_be_strictly_consecutive_and_nothing_is_written_for_a_bad_batch() {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    let writes = store.writes();

    // A gap after the last position.
    let error = relay.append_batch(batch(2, 5, 6), 2_000).await.unwrap_err();
    assert_eq!(
        error,
        RelayError::BatchGap {
            expected: 4,
            offered: 5
        }
    );
    assert_eq!(error.code(), "journal_relay.batch_gap");
    // A repeat of held positions.
    let error = relay.append_batch(batch(2, 3, 4), 2_000).await.unwrap_err();
    assert_eq!(
        error,
        RelayError::EntryOutOfOrder {
            expected: 4,
            offered: 3
        }
    );
    assert_eq!(error.code(), "journal_relay.entry_out_of_order");
    // A gap inside the batch.
    let mut inner_gap = batch(2, 4, 6);
    inner_gap.entries.remove(1);
    let error = relay.append_batch(inner_gap, 2_000).await.unwrap_err();
    assert_eq!(
        error,
        RelayError::BatchGap {
            expected: 5,
            offered: 6
        }
    );
    // A repeat inside the batch.
    let mut inner_repeat = batch(2, 4, 6);
    inner_repeat.entries[2].0 = pos(5);
    let error = relay.append_batch(inner_repeat, 2_000).await.unwrap_err();
    assert_eq!(
        error,
        RelayError::EntryOutOfOrder {
            expected: 6,
            offered: 5
        }
    );

    assert_eq!(store.writes(), writes, "a refused batch writes nothing");
    assert_eq!(relay.next_position(), pos(4));
    // The relay is not poisoned by a refusal.
    relay.append_batch(batch(2, 4, 6), 2_000).await.unwrap();
}

#[tokio::test]
async fn a_batch_is_all_or_nothing() {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    let writes = store.writes();
    // The third entry is bad: the first two must not be kept.
    let mut bad = batch(1, 1, 3);
    bad.entries[2].1 = b"two\nlines".to_vec();

    let error = relay.append_batch(bad, 2_000).await.unwrap_err();

    assert_eq!(error.code(), "journal_relay.entry_invalid");
    assert_eq!(store.writes(), writes);
    assert_eq!(relay.open_entries(), 0);
    assert_eq!(relay.next_position(), pos(1));
}

#[tokio::test]
async fn other_invalid_batches_are_refused() {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::Local)).await.unwrap();
    let empty = Batch {
        batch_id: 1,
        entries: Vec::new(),
    };
    assert_eq!(
        relay.append_batch(empty, 2_000).await.unwrap_err().code(),
        "journal_relay.entry_invalid"
    );
    // Another generation.
    let wrong = batch_in(2, 1, 1, 2);
    assert_eq!(
        relay.append_batch(wrong, 2_000).await.unwrap_err().code(),
        "journal_relay.entry_invalid"
    );
    // Not UTF-8.
    let mut bytes = batch(1, 1, 1);
    bytes.entries[0].1 = vec![0xff, 0xfe];
    assert_eq!(
        relay.append_batch(bytes, 2_000).await.unwrap_err().code(),
        "journal_relay.entry_invalid"
    );
    // A batch id that is not after the last.
    relay.append_batch(batch(5, 1, 2), 2_000).await.unwrap();
    assert_eq!(
        relay
            .append_batch(batch(5, 3, 4), 2_000)
            .await
            .unwrap_err()
            .code(),
        "journal_relay.entry_invalid"
    );
    assert_eq!(
        relay
            .append_batch(batch(4, 3, 4), 2_000)
            .await
            .unwrap_err()
            .code(),
        "journal_relay.entry_invalid"
    );
    // The first position of a generation is 1.
    let store = FaultStore::new();
    let mut fresh = open(&store, never_due(Durability::Local)).await.unwrap();
    assert_eq!(
        fresh.append_batch(batch(1, 2, 3), 2_000).await.unwrap_err(),
        RelayError::BatchGap {
            expected: 1,
            offered: 2
        }
    );
}

#[tokio::test]
async fn sealing_produces_a_chained_segment_and_a_manifest_that_verifies() {
    let store = FaultStore::new();
    let mut config = cfg(Durability::RemoteSync);
    config.limits = SealLimits {
        max_bytes: 1,
        max_age_millis: u64::MAX,
    };
    let mut relay = open(&store, config).await.unwrap();

    // Each append finds the previous batch due and seals it first.
    relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    relay.append_batch(batch(2, 4, 6), 3_000).await.unwrap();
    relay.append_batch(batch(3, 7, 9), 4_000).await.unwrap();
    assert!(relay.seal_now(5_000).await.unwrap());
    assert!(
        !relay.seal_now(5_000).await.unwrap(),
        "nothing left to seal"
    );

    let segments = stored_segments(&store, 1).await;
    assert_eq!(segments.len(), 3);
    let report = verify_chain(&segments).expect("the chain verifies");
    assert_eq!(report.entries, 9);
    assert_eq!(entries_of(&segments), input(1, 1, 9));

    let verified = stored_manifest(&store, 1).await;
    assert_eq!(verified.segments().len(), 3);
    assert_eq!(verified.last_seq(), 9);
    assert_eq!(verified.issued_at_millis(), 5_000);
    let last = verified.segments().last().unwrap();
    assert_eq!(last.segment_hash, report.head_hash);
    // Every chunk was cleaned up after its segment.
    assert!(store.keys_under(&tail_prefix(1)).await.is_empty());
    assert_eq!(relay.manifest().last_seq(), 9);
}

#[tokio::test]
async fn the_seal_steps_run_in_order_segment_manifest_chunks() {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::RemoteSync))
        .await
        .unwrap();
    relay.append_batch(batch(1, 1, 2), 2_000).await.unwrap();
    relay.append_batch(batch(2, 3, 4), 2_000).await.unwrap();
    store.clear_log();

    relay.seal_now(3_000).await.unwrap();

    let log = store.log();
    assert_eq!(log.len(), 4, "{log:?}");
    assert!(log[0].starts_with("put_new ") && log[0].contains("/segments/"));
    assert!(log[1].starts_with("put_replace ") && log[1].contains("/manifest/"));
    assert_eq!(log[2], format!("delete {}", tail_key(1, 1)));
    assert_eq!(log[3], format!("delete {}", tail_key(1, 2)));
}

#[tokio::test]
async fn an_old_open_segment_seals_by_age_and_an_idle_one_via_seal_if_due() {
    let store = FaultStore::new();
    let mut config = cfg(Durability::Local);
    config.limits = SealLimits {
        max_bytes: u64::MAX,
        max_age_millis: 1_000,
    };
    let mut relay = open(&store, config).await.unwrap();
    relay.append_batch(batch(1, 1, 2), 10_000).await.unwrap();
    assert!(!relay.seal_if_due(10_500).await.unwrap());
    assert!(relay.seal_if_due(11_000).await.unwrap());
    assert_eq!(relay.open_entries(), 0);
    assert_eq!(relay.manifest().last_seq(), 2);
    assert_eq!(
        entries_of(&stored_segments(&store, 1).await),
        input(1, 1, 2)
    );
    // Nothing waiting: nothing sealed, however late.
    assert!(!relay.seal_if_due(99_000).await.unwrap());
    assert_eq!(stored_segments(&store, 1).await.len(), 1);
}

#[tokio::test]
async fn open_rejects_an_unusable_configuration() {
    let store = FaultStore::new();
    for bad in [
        crate::RelayConfig::new("a/b", SOURCE, Durability::Local),
        crate::RelayConfig::new("", SOURCE, Durability::Local),
        crate::RelayConfig::new(TENANT, "  ", Durability::Local),
    ] {
        let error = open(&store, bad).await.err().expect("refused");
        assert_eq!(error.code(), "journal_relay.config_invalid");
    }
    assert_eq!(store.writes(), 0);
}
