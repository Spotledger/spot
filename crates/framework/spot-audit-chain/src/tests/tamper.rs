//! The tamper matrix: one test per way of altering a stored chain, each asserting the exact
//! `seq` and failure the verifier reports.

use futures::executor::block_on;

use super::*;
use crate::{AuditStore, ChainEntry, Checkpoint, Failure, FailureKind, PayloadValue, Verifier};

fn kinds(chain: &AuditChain<InMemoryStore>, ring: &KeyRing, to: u64) -> Vec<(u64, FailureKind)> {
    block_on(Verifier::new(ring).verify_range(chain.store(), SITE, 1, to))
        .unwrap()
        .failures
        .iter()
        .map(|f: &Failure| (f.seq, f.kind))
        .collect()
}

#[test]
fn an_untouched_chain_verifies() {
    let (chain, ring) = built(6, 3);
    assert_eq!(kinds(&chain, &ring, 6), []);
}

#[test]
fn a_payload_byte_changed() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| {
        e[2].record
            .payload
            .insert("note".into(), PayloadValue::Text("entry 4".into()));
    });
    assert_eq!(kinds(&chain, &ring, 6), [(3, FailureKind::HashMismatch)]);
}

#[test]
fn a_payload_integer_and_bool_changed() {
    let (chain, ring) = built(6, 0);
    mutate(&chain, |e| {
        e[1].record.payload.insert("n".into(), PayloadValue::Int(3));
        e[4].record
            .payload
            .insert("ok".into(), PayloadValue::Bool(true));
    });
    assert_eq!(
        kinds(&chain, &ring, 6),
        [
            (2, FailureKind::HashMismatch),
            (5, FailureKind::HashMismatch)
        ]
    );
}

#[test]
fn at_changed() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| e[3].record.at += 1);
    assert_eq!(kinds(&chain, &ring, 6), [(4, FailureKind::HashMismatch)]);
}

#[test]
fn actor_changed() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| e[1].record.actor = "user:u2".into());
    assert_eq!(kinds(&chain, &ring, 6), [(2, FailureKind::HashMismatch)]);
}

#[test]
fn kind_changed() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| e[0].record.kind = "doc.delete".into());
    assert_eq!(kinds(&chain, &ring, 6), [(1, FailureKind::HashMismatch)]);
}

#[test]
fn masked_and_subject_and_on_behalf_of_changed() {
    let (chain, ring) = built(6, 0);
    mutate(&chain, |e| {
        e[0].record.masked = false;
        e[1].record.subject = None;
        e[2].record.on_behalf_of = None;
    });
    assert_eq!(
        kinds(&chain, &ring, 6),
        [
            (1, FailureKind::HashMismatch),
            (2, FailureKind::HashMismatch),
            (3, FailureKind::HashMismatch)
        ]
    );
}

#[test]
fn seq_changed() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| e[2].seq = 9);
    let found = kinds(&chain, &ring, 9);
    assert!(found.contains(&(9, FailureKind::HashMismatch)), "{found:?}");
    assert!(found.contains(&(9, FailureKind::SeqGap)), "{found:?}");
}

#[test]
fn prev_hash_changed() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| e[3].prev_hash[0] ^= 1);
    let found = kinds(&chain, &ring, 6);
    assert!(found.contains(&(4, FailureKind::LinkBroken)), "{found:?}");
    assert!(found.contains(&(4, FailureKind::HashMismatch)), "{found:?}");
}

#[test]
fn a_stored_hash_changed() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| e[3].hash[5] ^= 0x80);
    let found = kinds(&chain, &ring, 6);
    assert!(found.contains(&(4, FailureKind::HashMismatch)), "{found:?}");
    assert!(found.contains(&(5, FailureKind::LinkBroken)), "{found:?}");
}

#[test]
fn the_first_entry_removed() {
    let (chain, ring) = built(6, 0);
    mutate(&chain, |e| {
        e.remove(0);
    });
    let found = kinds(&chain, &ring, 6);
    assert!(found.contains(&(2, FailureKind::SeqGap)), "{found:?}");
    assert!(found.contains(&(2, FailureKind::LinkBroken)), "{found:?}");
}

#[test]
fn a_middle_entry_removed() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| {
        e.remove(2);
    });
    let found = kinds(&chain, &ring, 6);
    assert!(found.contains(&(4, FailureKind::LinkBroken)), "{found:?}");
    assert!(found.contains(&(4, FailureKind::SeqGap)), "{found:?}");
    assert_eq!(found.len(), 2);
}

#[test]
fn two_entries_swapped() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| e.swap(2, 3));
    let found = kinds(&chain, &ring, 6);
    assert!(found.contains(&(4, FailureKind::LinkBroken)), "{found:?}");
    assert!(found.contains(&(3, FailureKind::LinkBroken)), "{found:?}");
}

#[test]
fn an_entry_duplicated() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| {
        let copy = e[2].clone();
        e.insert(3, copy);
    });
    assert_eq!(kinds(&chain, &ring, 6), [(3, FailureKind::Duplicate)]);
}

#[test]
fn a_forged_entry_inserted_with_a_valid_hash() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| {
        // Correctly linked and hashed after entry 3, but entry 4 still follows the real 3.
        let forged = ChainEntry::new(4, e[2].hash, record(99));
        e.insert(3, forged);
    });
    let found = kinds(&chain, &ring, 6);
    assert!(found.contains(&(4, FailureKind::Duplicate)), "{found:?}");
    assert!(!found.is_empty());
}

#[test]
fn a_tail_truncated_with_a_surviving_checkpoint() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| e.truncate(4));
    assert_eq!(
        kinds(&chain, &ring, 6),
        [(6, FailureKind::CheckpointAheadOfChain)]
    );
}

#[test]
fn a_tail_replaced_by_a_different_one_is_caught_by_the_checkpoint_hash() {
    let (chain, ring) = built(6, 3);
    mutate(&chain, |e| {
        e.truncate(5);
        let forged = ChainEntry::new(6, e[4].hash, record(99));
        e.push(forged);
    });
    assert_eq!(
        kinds(&chain, &ring, 6),
        [(6, FailureKind::CheckpointAheadOfChain)]
    );
}

#[test]
fn a_tail_truncated_without_a_surviving_checkpoint_needs_an_external_checkpoint() {
    let (chain, ring) = built(6, 3);
    let trusted = block_on(chain.store().latest_checkpoint(SITE))
        .unwrap()
        .unwrap();
    mutate(&chain, |e| e.truncate(4));
    chain.store().state.lock().unwrap().checkpoints.clear();
    // The chain alone is internally consistent: that is the inherent limit.
    assert_eq!(kinds(&chain, &ring, 6), []);
    // A checkpoint held elsewhere (a nightly run's last trusted one) exposes it.
    let report =
        block_on(Verifier::new(&ring).verify_incremental(chain.store(), SITE, &trusted)).unwrap();
    assert_eq!(
        report.failures,
        [Failure {
            seq: 6,
            kind: FailureKind::CheckpointAheadOfChain,
            record_kind: None
        }]
    );
}

#[test]
fn a_forged_checkpoint_fails_its_signature() {
    let (chain, ring) = built(6, 3);
    // Altering any signed field.
    chain
        .store()
        .state
        .lock()
        .unwrap()
        .checkpoints
        .get_mut(SITE)
        .unwrap()
        .at += 1;
    assert_eq!(
        kinds(&chain, &ring, 6),
        [(6, FailureKind::BadCheckpointSignature)]
    );

    // A flipped signature byte, and a signature of the wrong length.
    let (chain, ring) = built(6, 3);
    chain
        .store()
        .state
        .lock()
        .unwrap()
        .checkpoints
        .get_mut(SITE)
        .unwrap()
        .signature[0] ^= 1;
    assert_eq!(
        kinds(&chain, &ring, 6),
        [(6, FailureKind::BadCheckpointSignature)]
    );
    let (chain, ring) = built(6, 3);
    chain
        .store()
        .state
        .lock()
        .unwrap()
        .checkpoints
        .get_mut(SITE)
        .unwrap()
        .signature
        .pop();
    assert_eq!(
        kinds(&chain, &ring, 6),
        [(6, FailureKind::BadCheckpointSignature)]
    );
}

#[test]
fn a_checkpoint_for_another_site_is_refused() {
    let (chain, ring) = built(6, 3);
    let head = block_on(chain.head()).unwrap().unwrap();
    let foreign = Checkpoint::sign(&*signer(1), "other-site", head.seq, head.hash, 1).unwrap();
    assert_eq!(
        Verifier::new(&ring).check_checkpoint(SITE, &foreign),
        Some(FailureKind::BadCheckpointSignature)
    );
}

#[test]
fn a_checkpoint_signed_by_an_unknown_key() {
    let (chain, ring) = built(6, 3);
    let head = block_on(chain.head()).unwrap().unwrap();
    let stranger = signer(9);
    let forged = Checkpoint::sign(&*stranger, SITE, head.seq, head.hash, 1).unwrap();
    block_on(chain.store().put_checkpoint(forged)).unwrap();
    assert_eq!(kinds(&chain, &ring, 6), [(6, FailureKind::UnknownKey)]);
}

#[test]
fn a_range_in_the_middle_is_anchored_to_the_entry_before_it() {
    let (chain, ring) = built(8, 0);
    let v = Verifier::new(&ring);
    assert!(
        block_on(v.verify_range(chain.store(), SITE, 4, 6))
            .unwrap()
            .is_ok()
    );
    mutate(&chain, |e| e[2].record.at += 1); // the anchor, seq 3
    let report = block_on(v.verify_range(chain.store(), SITE, 4, 6)).unwrap();
    assert!(report.has(FailureKind::HashMismatch));
    let (chain, ring) = built(8, 0);
    mutate(&chain, |e| e[3].prev_hash[0] ^= 1); // seq 4 no longer links to 3
    let report = block_on(Verifier::new(&ring).verify_range(chain.store(), SITE, 4, 6)).unwrap();
    assert!(report.has(FailureKind::LinkBroken));
}
