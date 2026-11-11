//! Append, read, checkpoint, validation, concurrency, rotation and the store contract.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::executor::block_on;

use super::*;
use crate::contract;
use crate::{
    AppendOutcome, AuditError, AuditStore, Checkpoint, FailureKind, StoreFuture, Verifier,
    genesis_hash, segment_bounds,
};

#[test]
fn append_assigns_gap_free_seq_and_links_to_genesis() {
    let (chain, _) = built(3, 0);
    let all = block_on(chain.read_range(1, 3)).unwrap();
    assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(all[0].prev_hash, genesis_hash(SITE));
    assert_eq!(all[1].prev_hash, all[0].hash);
    assert_eq!(all[2].prev_hash, all[1].hash);
    assert_eq!(block_on(chain.head()).unwrap(), Some(all[2].clone()));
    assert_eq!(
        block_on(chain.read_range(2, 2)).unwrap(),
        vec![all[1].clone()]
    );
}

#[test]
fn ten_thousand_entries_append_and_verify() {
    let (chain, ring) = built(10_000, 1000);
    let report =
        block_on(Verifier::new(&ring).verify_range(chain.store(), SITE, 1, 10_000)).unwrap();
    assert!(report.is_ok(), "{:?}", report.failures);
    assert_eq!(report.checked, 10_000);
    let cp = block_on(chain.store().latest_checkpoint(SITE))
        .unwrap()
        .unwrap();
    assert_eq!(cp.seq, 10_000);
}

#[test]
fn checkpoints_follow_the_policy_and_on_demand() {
    let (chain, ring) = built(7, 3);
    let cp = block_on(chain.store().latest_checkpoint(SITE))
        .unwrap()
        .unwrap();
    assert_eq!(cp.seq, 6);
    let now = block_on(chain.checkpoint_now(42)).unwrap().unwrap();
    assert_eq!((now.seq, now.at), (7, 42));
    assert_eq!(Verifier::new(&ring).check_checkpoint(SITE, &now), None);
    let empty = AuditChain::new(
        InMemoryStore::new(),
        SITE,
        signer(1),
        CheckpointPolicy::default(),
    );
    assert_eq!(block_on(empty.checkpoint_now(1)).unwrap(), None);
}

#[test]
fn a_record_is_validated_before_it_is_appended() {
    let (chain, _) = built(0, 0);
    let try_with = |f: &dyn Fn(&mut AuditRecord)| {
        let mut r = record(1);
        f(&mut r);
        block_on(chain.append(r)).unwrap_err()
    };
    assert_eq!(
        try_with(&|r| r.kind = "Doc.Update".into()).code(),
        "audit.record_invalid"
    );
    assert_eq!(
        try_with(&|r| r.kind = String::new()).code(),
        "audit.record_invalid"
    );
    assert_eq!(
        try_with(&|r| r.actor = "bob".into()).code(),
        "audit.record_invalid"
    );
    assert_eq!(
        try_with(&|r| r.actor = "user:".into()).code(),
        "audit.record_invalid"
    );
    assert_eq!(
        try_with(&|r| r.on_behalf_of = Some("nobody".into())).code(),
        "audit.record_invalid"
    );
    assert_eq!(
        try_with(&|r| {
            r.payload.insert(String::new(), PayloadValue::Bool(true));
        })
        .code(),
        "audit.record_invalid"
    );
    assert_eq!(block_on(chain.head()).unwrap(), None);
}

#[test]
fn floats_are_refused() {
    let err = PayloadValue::from_f64(1.5).unwrap_err();
    assert_eq!(err, AuditError::FloatRefused);
    assert_eq!(err.code(), "audit.float_refused");
}

#[test]
fn the_payload_limit_is_exactly_eight_kib_encoded() {
    // count(4) + field(key "k")(5) + tag(1) + field(text)(4 + len)
    let with_text = |len: usize| {
        let mut r = record(1);
        r.payload = Payload::new();
        r.payload
            .insert("k".into(), PayloadValue::Text("a".repeat(len)));
        r
    };
    assert_eq!(crate::payload_encoded_len(&with_text(8178).payload), 8192);
    assert!(with_text(8178).validate().is_ok());
    let err = with_text(8179).validate().unwrap_err();
    assert_eq!(err.code(), "audit.payload_too_large");
    assert_eq!(
        err,
        AuditError::PayloadTooLarge {
            size: 8193,
            limit: 8192
        }
    );
}

/// A store whose `head` yields once, so two writers both read the same head before either writes.
struct Racy(InMemoryStore);

struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl AuditStore for Racy {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async move {
            let head = self.0.head(site).await;
            YieldOnce(false).await;
            head
        })
    }
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.0.append_if_head(site, expected_seq, entry)
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        self.0.range(site, from, to)
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.0.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.0.latest_checkpoint(site)
    }
}

#[test]
fn concurrent_writers_produce_one_gap_free_chain() {
    let s = signer(1);
    let ring = ring(&[&s]);
    let chain = AuditChain::new(
        Racy(InMemoryStore::new()),
        SITE,
        s,
        CheckpointPolicy { every: 0 },
    );
    let (a, b, c) = block_on(async {
        futures::join!(
            chain.append(record(1)),
            chain.append(record(2)),
            chain.append(record(3))
        )
    });
    let mut seqs = [a.unwrap().seq, b.unwrap().seq, c.unwrap().seq];
    seqs.sort_unstable();
    assert_eq!(seqs, [1, 2, 3]);
    let report = block_on(Verifier::new(&ring).verify_range(chain.store(), SITE, 1, 3)).unwrap();
    assert!(report.is_ok(), "{:?}", report.failures);
    assert_eq!(report.checked, 3);
}

#[test]
fn too_many_lost_races_give_head_conflict() {
    /// A store that always says the head moved.
    struct Never(InMemoryStore);
    impl AuditStore for Never {
        fn head<'a>(&'a self, s: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
            self.0.head(s)
        }
        fn append_if_head<'a>(
            &'a self,
            _: &'a str,
            _: u64,
            _: ChainEntry,
        ) -> StoreFuture<'a, AppendOutcome> {
            Box::pin(async { Ok(AppendOutcome::HeadMoved) })
        }
        fn range<'a>(&'a self, s: &'a str, f: u64, t: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
            self.0.range(s, f, t)
        }
        fn put_checkpoint<'a>(&'a self, c: Checkpoint) -> StoreFuture<'a, ()> {
            self.0.put_checkpoint(c)
        }
        fn latest_checkpoint<'a>(&'a self, s: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
            self.0.latest_checkpoint(s)
        }
    }
    let chain = AuditChain::new(
        Never(InMemoryStore::new()),
        SITE,
        signer(1),
        CheckpointPolicy::default(),
    );
    let err = block_on(chain.append(record(1))).unwrap_err();
    assert_eq!(err.code(), "audit.head_conflict");
}

#[test]
fn a_checkpoint_from_the_old_key_still_verifies_after_rotation() {
    let (chain, ring_a) = built(4, 2);
    let old = block_on(chain.store().latest_checkpoint(SITE))
        .unwrap()
        .unwrap();
    let head = block_on(chain.head()).unwrap().unwrap();
    // Rotate: a new key signs the newest checkpoint; the ring keeps both public keys.
    let new_signer = signer(2);
    let new_cp = Checkpoint::sign(&*new_signer, SITE, head.seq, head.hash, 99).unwrap();
    block_on(chain.store().put_checkpoint(new_cp)).unwrap();
    let mut both = ring_a.clone();
    both.add(&new_signer.public_key()).unwrap();
    let v = Verifier::new(&both);
    assert!(
        block_on(v.verify_range(chain.store(), SITE, 1, 4))
            .unwrap()
            .is_ok()
    );
    assert!(
        block_on(v.verify_incremental(chain.store(), SITE, &old))
            .unwrap()
            .is_ok()
    );
    // A ring that dropped the old key can no longer vouch for it.
    let only_new = ring(&[&new_signer]);
    let report =
        block_on(Verifier::new(&only_new).verify_incremental(chain.store(), SITE, &old)).unwrap();
    assert!(report.has(FailureKind::UnknownKey));
}

#[test]
fn incremental_verification_checks_only_what_is_new() {
    let (chain, ring) = built(5, 5);
    let trusted = block_on(chain.store().latest_checkpoint(SITE))
        .unwrap()
        .unwrap();
    for i in 6..=10 {
        block_on(chain.append(record(i))).unwrap();
    }
    let v = Verifier::new(&ring);
    let report = block_on(v.verify_incremental(chain.store(), SITE, &trusted)).unwrap();
    assert!(report.is_ok(), "{:?}", report.failures);
    assert_eq!((report.from, report.checked), (6, 5));

    // Alteration after the checkpoint is found; before it, only the full verification finds it.
    mutate(&chain, |e| e[7].record.at += 1);
    let report = block_on(v.verify_incremental(chain.store(), SITE, &trusted)).unwrap();
    assert_eq!(report.failures.len(), 1);
    assert_eq!(
        (report.failures[0].seq, report.failures[0].kind),
        (8, FailureKind::HashMismatch)
    );
    mutate(&chain, |e| e[7].record.at -= 1);
    mutate(&chain, |e| e[2].record.at += 1);
    assert!(
        block_on(v.verify_incremental(chain.store(), SITE, &trusted))
            .unwrap()
            .is_ok()
    );
    assert!(
        block_on(v.verify_range(chain.store(), SITE, 1, 10))
            .unwrap()
            .has(FailureKind::HashMismatch)
    );
}

#[test]
fn segments_verify_on_their_own() {
    assert_eq!(segment_bounds(0, 5), Some((1, 5)));
    assert_eq!(segment_bounds(2, 5), Some((11, 15)));
    assert_eq!(segment_bounds(0, 0), None);
    let (chain, ring) = built(15, 5);
    let v = Verifier::new(&ring);
    mutate(&chain, |e| e[2].record.at += 1); // seq 3, in segment 0
    let seg0 = block_on(v.verify_segment(chain.store(), SITE, 0, 5)).unwrap();
    let seg1 = block_on(v.verify_segment(chain.store(), SITE, 1, 5)).unwrap();
    assert!(seg0.has(FailureKind::HashMismatch));
    assert!(seg1.is_ok(), "{:?}", seg1.failures);
    assert_eq!((seg1.from, seg1.to, seg1.checked), (6, 10, 5));
}

#[test]
fn a_report_carries_seq_kind_and_failure_never_the_payload() {
    let (chain, ring) = built(3, 0);
    mutate(&chain, |e| {
        e[1].record
            .payload
            .insert("note".into(), PayloadValue::Text("SECRET-VALUE".into()));
    });
    let report = block_on(Verifier::new(&ring).verify_range(chain.store(), SITE, 1, 3)).unwrap();
    assert_eq!(report.failures.len(), 1);
    assert_eq!(
        report.failures[0].record_kind.as_deref(),
        Some("doc.update")
    );
    let text = format!("{report:?}");
    assert!(!text.contains("SECRET-VALUE") && !text.contains("entry 2"));
}

#[test]
fn failure_codes_are_the_catalogue_codes() {
    let all = [
        (FailureKind::HashMismatch, "audit.hash_mismatch"),
        (FailureKind::LinkBroken, "audit.link_broken"),
        (FailureKind::SeqGap, "audit.seq_gap"),
        (FailureKind::Duplicate, "audit.duplicate"),
        (
            FailureKind::BadCheckpointSignature,
            "audit.bad_checkpoint_signature",
        ),
        (
            FailureKind::CheckpointAheadOfChain,
            "audit.checkpoint_ahead_of_chain",
        ),
        (FailureKind::UnknownKey, "audit.unknown_key"),
    ];
    for (kind, code) in all {
        assert_eq!(kind.code(), code);
    }
}

#[test]
fn the_in_memory_store_passes_the_contract_suite() {
    block_on(contract::run(&InMemoryStore::new(), "t1")).unwrap();
}

#[test]
fn key_ring_refuses_a_bad_public_key() {
    let mut ring = KeyRing::new();
    assert_eq!(
        ring.add(&[1, 2, 3]).unwrap_err().code(),
        "audit.key_invalid"
    );
}
