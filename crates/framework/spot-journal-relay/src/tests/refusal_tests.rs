//! `resume` refuses a stored state that disagrees with the manifest, and never overwrites.

use spot_journal_manifest::VerifyingKeys;
use spot_journal_segment::{SegmentConfig, SegmentWriter, genesis_hash, segment_hash};
use spot_journal_store::Layout;

use super::support::*;
use crate::chunk;
use crate::{Durability, Relay, RelayError};

/// A store holding two sealed segments (1..=3 and 4..=6); returns it with the head hash.
async fn sealed_store() -> (FaultStore, [u8; 32]) {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(Durability::Local)).await.unwrap();
    relay.append_batch(batch(1, 1, 3), 2_000).await.unwrap();
    relay.seal_now(3_000).await.unwrap();
    relay.append_batch(batch(2, 4, 6), 4_000).await.unwrap();
    relay.seal_now(5_000).await.unwrap();
    let head = stored_manifest(&store, 1)
        .await
        .segments()
        .last()
        .unwrap()
        .segment_hash;
    (store, head)
}

/// A copy of everything under the tenant except the keys `skip` says to leave out.
async fn copy_except(from: &FaultStore, skip: impl Fn(&str) -> bool) -> FaultStore {
    let to = FaultStore::new();
    for key in from.keys_under(&format!("{TENANT}/")).await {
        if !skip(&key) {
            to.put(&key, from.read(&key).await.unwrap()).await;
        }
    }
    to
}

fn segment(first: u64, last: u64, prev: [u8; 32], source: &str) -> Vec<u8> {
    let mut writer = SegmentWriter::new(SegmentConfig::after(source, 1, prev)).unwrap();
    for seq in first..=last {
        writer.push(pos(seq), &line(1, seq)).unwrap();
    }
    writer.seal(Vec::<String>::new()).unwrap().bytes
}

fn segment_key(first: u64, last: u64) -> String {
    Layout::segment(TENANT, 1, first, last).unwrap().to_key()
}

async fn reopen(store: &FaultStore) -> Result<Relay<FaultStore>, RelayError> {
    open(store, never_due(Durability::RemoteSync)).await
}

#[tokio::test]
async fn a_healthy_store_reopens() {
    let (store, _) = sealed_store().await;
    let relay = reopen(&store).await.unwrap();
    assert_eq!(relay.next_position(), pos(7));
}

#[tokio::test]
async fn a_segment_that_differs_from_the_manifest_is_chain_broken_and_nothing_is_written() {
    let (store, head) = sealed_store().await;
    let last = segment_key(4, 6);
    let tampered = copy_except(&store, |k| k == last).await;
    // Same key, other bytes (a different line).
    let mut writer = SegmentWriter::new(SegmentConfig::after(
        SOURCE,
        1,
        stored_manifest(&store, 1).await.segments()[0].segment_hash,
    ))
    .unwrap();
    for seq in 4..=6 {
        writer.push(pos(seq), b"{\"forged\":true}").unwrap();
    }
    let forged = writer.seal(Vec::<String>::new()).unwrap().bytes;
    assert_ne!(segment_hash(&forged), head);
    tampered.put(&last, forged).await;
    let before = tampered.read(&manifest_key(1)).await.unwrap();
    let writes = tampered.writes();

    let error = reopen(&tampered).await.err().expect("refused");

    assert_eq!(error.code(), "journal_relay.chain_broken");
    assert_eq!(tampered.read(&manifest_key(1)).await.unwrap(), before);
    assert_eq!(
        tampered.writes(),
        writes,
        "nothing is overwritten or deleted"
    );
}

#[tokio::test]
async fn a_manifest_segment_missing_from_the_store_is_chain_broken() {
    let (store, _) = sealed_store().await;
    let first = segment_key(1, 3);
    let missing = copy_except(&store, |k| k == first).await;
    let error = reopen(&missing).await.err().expect("refused");
    assert_eq!(error.code(), "journal_relay.chain_broken");
}

#[tokio::test]
async fn a_manifest_the_keys_do_not_verify_is_chain_broken() {
    let (store, _) = sealed_store().await;
    let error = Relay::open(
        store.clone(),
        Box::new(FixedSigner),
        VerifyingKeys::new(),
        never_due(Durability::Local),
        9_000,
    )
    .await
    .err()
    .expect("refused");
    assert_eq!(error.code(), "journal_relay.chain_broken");
}

#[tokio::test]
async fn an_unlisted_segment_that_chains_is_adopted_after_it_is_verified() {
    let (store, head) = sealed_store().await;
    store
        .put(&segment_key(7, 9), segment(7, 9, head, SOURCE))
        .await;

    let relay = reopen(&store).await.unwrap();

    assert_eq!(relay.manifest().last_seq(), 9);
    assert_eq!(relay.next_position(), pos(10));
    let manifest = stored_manifest(&store, 1).await;
    assert_eq!(manifest.segments().len(), 3);
}

#[tokio::test]
async fn an_unlisted_segment_that_does_not_chain_is_chain_broken() {
    let (store, head) = sealed_store().await;
    // Wrong previous hash.
    let wrong_chain = copy_except(&store, |_| false).await;
    wrong_chain
        .put(&segment_key(7, 9), segment(7, 9, [9; 32], SOURCE))
        .await;
    assert_eq!(
        reopen(&wrong_chain).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );
    // Another source.
    let other_source = copy_except(&store, |_| false).await;
    other_source
        .put(&segment_key(7, 9), segment(7, 9, head, "other-site"))
        .await;
    assert_eq!(
        reopen(&other_source).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );
    // A gap after the manifest.
    let gap = copy_except(&store, |_| false).await;
    gap.put(&segment_key(20, 22), segment(20, 22, head, SOURCE))
        .await;
    assert_eq!(
        reopen(&gap).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );
    // A key that says one range and bytes that hold another.
    let lying = copy_except(&store, |_| false).await;
    lying
        .put(&segment_key(7, 9), segment(7, 8, head, SOURCE))
        .await;
    assert_eq!(
        reopen(&lying).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );
    // Damaged bytes.
    let damaged = copy_except(&store, |_| false).await;
    let mut bytes = segment(7, 9, head, SOURCE);
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    damaged.put(&segment_key(7, 9), bytes).await;
    assert_eq!(
        reopen(&damaged).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );
}

#[tokio::test]
async fn a_stray_segment_below_the_manifest_end_does_not_make_the_manifest_untrue() {
    let (store, _) = sealed_store().await;
    // A losing relay's orphan: a range the manifest does not list, ending inside it.
    store
        .put(
            &segment_key(1, 2),
            segment(1, 2, genesis_hash(SOURCE), SOURCE),
        )
        .await;
    let relay = reopen(&store).await.unwrap();
    assert_eq!(relay.next_position(), pos(7));
}

#[tokio::test]
async fn damaged_gapped_or_straddling_tail_chunks_are_chain_broken() {
    let (store, _) = sealed_store().await;

    let damaged = copy_except(&store, |_| false).await;
    let mut bytes = chunk::encode(&input(1, 7, 9)).unwrap();
    bytes[12] ^= 0x01;
    damaged.put(&tail_key(1, 3), bytes).await;
    assert_eq!(
        reopen(&damaged).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );

    let gap = copy_except(&store, |_| false).await;
    gap.put(&tail_key(1, 3), chunk::encode(&input(1, 8, 9)).unwrap())
        .await;
    assert_eq!(
        reopen(&gap).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );

    let straddle = copy_except(&store, |_| false).await;
    straddle
        .put(&tail_key(1, 3), chunk::encode(&input(1, 5, 8)).unwrap())
        .await;
    assert_eq!(
        reopen(&straddle).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );

    let wrong_generation = copy_except(&store, |_| false).await;
    wrong_generation
        .put(&tail_key(1, 3), chunk::encode(&input(2, 7, 9)).unwrap())
        .await;
    assert_eq!(
        reopen(&wrong_generation).await.err().unwrap().code(),
        "journal_relay.chain_broken"
    );
}

#[tokio::test]
async fn chunks_the_manifest_covers_are_deleted_and_the_rest_are_sealed() {
    let (store, _) = sealed_store().await;
    // A covered chunk (4..=6) left behind by a crash before step (c), and a new one (7..=8).
    store
        .put(&tail_key(1, 2), chunk::encode(&input(1, 4, 6)).unwrap())
        .await;
    store
        .put(&tail_key(1, 3), chunk::encode(&input(1, 7, 8)).unwrap())
        .await;

    let relay = reopen(&store).await.unwrap();

    assert_eq!(relay.manifest().last_seq(), 8);
    assert!(store.keys_under(&tail_prefix(1)).await.is_empty());
    let segments = stored_segments(&store, 1).await;
    assert_eq!(entries_of(&segments), input(1, 1, 8));
}

#[tokio::test]
async fn a_missing_manifest_over_verified_segments_is_refused_and_nothing_is_written() {
    let (store, _) = sealed_store().await;
    let without = copy_except(&store, |k| k == manifest_key(1)).await;
    let before = without.keys_under("").await;
    // `open` no longer signs a fresh manifest beside segments it does not list (WP-JRN-03.T6).
    let error = reopen(&without).await.err().expect("refused");
    assert_eq!(error.code(), "journal_relay.chain_broken");
    assert_eq!(without.keys_under("").await, before);
    assert!(without.read(&manifest_key(1)).await.is_none());
}
