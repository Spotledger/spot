#![allow(clippy::expect_used, clippy::unwrap_used)]
//! A refused entry's mark never outlives the sink's custody of it: the mark is
//! *offered* when the hub hands the entry to the sink and *held* only when the sink confirms it
//! has it, and a sink that fails leaves the entry deferred and offered again on redelivery.

use std::collections::BTreeSet;

use spot_shared_balance::{
    AllocatedQuota, BalanceId, BalancePolicy, Batch, Entry, EntryId, EntryKind, Hub, Mark, Millis,
    NodeId, QuarantineSink, RejectReason, SinkError, VecQuarantine,
};

fn node(name: &str) -> NodeId {
    NodeId::new(name)
}

fn hub() -> Hub {
    let mut hub = Hub::new(node("HO"));
    hub.open_balance(BalanceId::new("card"), 100, Millis(0))
        .expect("open");
    hub
}

/// A debit on a balance head office does not hold: always refused as `UnknownBalance`.
fn refused(who: &str, seq: u64) -> Entry {
    Entry::new(
        EntryId {
            node: node(who),
            seq,
        },
        BalanceId::new("no-such-card"),
        EntryKind::Debit,
        5,
        Millis(seq),
    )
    .expect("entry")
}

fn batch(who: &str, entries: Vec<Entry>) -> Batch {
    Batch {
        node: node(who),
        entries,
        complete: false,
    }
}

/// A sink that buffers: `Ok` does not mean custody, so the caller confirms later.
#[derive(Default)]
struct Buffering {
    offered: Vec<EntryId>,
}

impl QuarantineSink for Buffering {
    fn quarantine(&mut self, entry: &Entry, _reason: RejectReason) -> Result<(), SinkError> {
        self.offered.push(entry.id.clone());
        Ok(())
    }

    fn confirms_on_accept(&self) -> bool {
        false
    }
}

#[test]
fn a_sink_that_fails_once_gets_the_entry_again_on_redelivery_exactly_once() {
    let mut hub = hub();
    let policy = AllocatedQuota;
    let entry = refused("x", 1);
    let delivery = batch("x", vec![entry.clone()]);
    let mut sink = VecQuarantine::failing(1);

    let first = policy
        .settle(&mut hub, &delivery, Millis(5), &mut sink)
        .expect("settle");
    assert_eq!(first.deferred, vec![entry.id.clone()]);
    assert!(first.quarantined.is_empty());
    assert!(
        !first.handled().contains(&entry.id),
        "an entry the sink did not take is not acknowledged"
    );
    assert!(sink.held.is_empty());
    assert_eq!(hub.mark(&entry.id), Some(Mark::Offered));
    assert!(!hub.is_quarantined(&entry.id));
    assert_eq!(hub.offered_unconfirmed(), vec![entry.id.clone()]);

    // The sender redelivers; the sink works now and takes the entry once.
    let second = policy
        .settle(&mut hub, &delivery, Millis(6), &mut sink)
        .expect("settle");
    assert_eq!(second.quarantined.len(), 1);
    assert!(second.deferred.is_empty());
    assert!(second.handled().contains(&entry.id));
    assert_eq!(sink.held.len(), 1);
    assert_eq!(hub.mark(&entry.id), Some(Mark::Held));
    assert!(hub.is_quarantined(&entry.id));
    assert!(hub.offered_unconfirmed().is_empty());

    // A repeat after the sink confirmed custody does not call the sink again.
    let third = policy
        .settle(&mut hub, &delivery, Millis(7), &mut sink)
        .expect("settle");
    assert_eq!(third.duplicates, vec![entry.id.clone()]);
    assert!(third.quarantined.is_empty() && third.deferred.is_empty());
    assert_eq!(sink.held.len(), 1, "the sink was not offered it again");
}

#[test]
fn a_buffering_sink_is_offered_the_entry_until_the_caller_confirms_custody() {
    let mut hub = hub();
    let policy = AllocatedQuota;
    let entry = refused("x", 1);
    let delivery = batch("x", vec![entry.clone()]);
    let mut sink = Buffering::default();

    let first = policy
        .settle(&mut hub, &delivery, Millis(5), &mut sink)
        .expect("settle");
    assert_eq!(first.quarantined.len(), 1, "accepted for custody");
    assert_eq!(hub.mark(&entry.id), Some(Mark::Offered), "not held yet");
    assert_eq!(hub.offered_unconfirmed(), vec![entry.id.clone()]);

    // The buffer is lost (a restart): the redelivery offers the same entry to the new sink.
    let mut replacement = Buffering::default();
    let again = policy
        .settle(&mut hub, &delivery, Millis(6), &mut replacement)
        .expect("settle");
    assert_eq!(again.quarantined.len(), 1);
    assert_eq!(replacement.offered, vec![entry.id.clone()]);

    // The queue write completes: now it is a repeat.
    assert!(hub.confirm_held(&entry.id));
    assert!(hub.is_quarantined(&entry.id));
    let repeat = policy
        .settle(&mut hub, &delivery, Millis(7), &mut replacement)
        .expect("settle");
    assert_eq!(repeat.duplicates, vec![entry.id.clone()]);
    assert_eq!(replacement.offered.len(), 1);

    // An id the hub never offered cannot be confirmed.
    assert!(!hub.confirm_held(&refused("x", 99).id));
}

#[test]
fn a_foreign_entry_is_never_marked_even_when_the_sink_fails() {
    let mut hub = hub();
    let policy = AllocatedQuota;
    let foreign = refused("other", 1);
    // The batch says node "x", the entry belongs to node "other".
    let delivery = batch("x", vec![foreign.clone()]);

    let mut working = VecQuarantine::default();
    let outcome = policy
        .settle(&mut hub, &delivery, Millis(5), &mut working)
        .expect("settle");
    assert_eq!(outcome.rejected_foreign, vec![foreign.id.clone()]);
    assert_eq!(hub.mark(&foreign.id), None, "not marked");

    let mut failing = VecQuarantine::failing(1);
    let outcome = policy
        .settle(&mut hub, &delivery, Millis(6), &mut failing)
        .expect("settle");
    assert!(outcome.rejected_foreign.is_empty());
    assert_eq!(outcome.deferred, vec![foreign.id.clone()]);
    assert_eq!(hub.mark(&foreign.id), None);
}

#[test]
fn the_sink_error_names_a_stable_code_and_no_entry() {
    let error = SinkError;
    assert_eq!(error.code(), "shared_balance.sink_failed");
    assert_eq!(
        error.to_string(),
        "the quarantine sink could not take the entry"
    );
}

/// A small deterministic generator, so a failing seed reproduces.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// A sink that fails at random and keeps what it takes.
struct Flaky<'a> {
    rng: &'a mut Lcg,
    held: Vec<EntryId>,
}

impl QuarantineSink for Flaky<'_> {
    fn quarantine(&mut self, entry: &Entry, _reason: RejectReason) -> Result<(), SinkError> {
        if self.rng.next().is_multiple_of(3) {
            return Err(SinkError);
        }
        self.held.push(entry.id.clone());
        Ok(())
    }
}

#[test]
fn six_seeds_of_random_sink_failures_lose_nothing_and_queue_nothing_twice() {
    for seed in 1_u64..=6 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9));
        let mut hub = hub();
        let policy = AllocatedQuota;
        let entries: Vec<Entry> = (1..=20).map(|seq| refused("x", seq)).collect();
        let delivery = batch("x", entries.clone());
        let wanted: BTreeSet<EntryId> = entries.iter().map(|e| e.id.clone()).collect();

        let mut taken: Vec<EntryId> = Vec::new();
        let mut acknowledged: BTreeSet<EntryId> = BTreeSet::new();
        // The sender keeps redelivering whatever was not acknowledged.
        for round in 0..200_u64 {
            let remaining: Vec<Entry> = entries
                .iter()
                .filter(|e| !acknowledged.contains(&e.id))
                .cloned()
                .collect();
            if remaining.is_empty() {
                break;
            }
            let mut sink = Flaky {
                rng: &mut rng,
                held: Vec::new(),
            };
            let outcome = policy
                .settle(
                    &mut hub,
                    &batch("x", remaining),
                    Millis(10 + round),
                    &mut sink,
                )
                .expect("settle");
            taken.extend(sink.held);
            acknowledged.extend(outcome.handled());
        }
        assert_eq!(acknowledged, wanted, "seed {seed}: every entry was handled");
        let distinct: BTreeSet<&EntryId> = taken.iter().collect();
        assert_eq!(
            distinct.len(),
            taken.len(),
            "seed {seed}: no entry reached the sink twice"
        );
        assert_eq!(
            taken.len(),
            wanted.len(),
            "seed {seed}: no refused entry was lost"
        );
        assert!(hub.offered_unconfirmed().is_empty(), "seed {seed}");
        let _ = &delivery;
    }
}
