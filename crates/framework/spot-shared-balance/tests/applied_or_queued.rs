#![allow(clippy::expect_used)]
//! Recorded security finding: an entry id is applied or queued, never both.
//!
//! A refusal is two steps for head office: it marks the entry *offered* when it hands it to the
//! quarantine sink, and *held* only when the sink confirms custody. Between those two steps the
//! entry is not on the ledger, so a redelivery used to be re-adjudicated — and once head office's
//! state had moved (a top-up, a closed quota) the entry could be applied while its pending
//! quarantine copy was still on its way to the queue. It is now completed into the queue instead.

use spot_shared_balance::{
    BalanceId, BalancePolicy, Batch, Confirmation, Confirmer, Entry, EntryId, EntryKind, Hub, Mark,
    Millis, NodeId, OnlineOnly, QuarantineSink, RejectReason, SinkError, VecQuarantine,
};

fn node(name: &str) -> NodeId {
    NodeId::new(name)
}

fn card() -> BalanceId {
    BalanceId::new("gift-card-1")
}

/// A sink that buffers: `Ok` means "queued in memory", not "in custody", so the hub's mark stays
/// *offered* until the caller confirms the queue write.
#[derive(Default)]
struct Buffering {
    offered: Vec<(EntryId, RejectReason)>,
}

impl QuarantineSink for Buffering {
    fn quarantine(&mut self, entry: &Entry, reason: RejectReason) -> Result<(), SinkError> {
        self.offered.push((entry.id.clone(), reason));
        Ok(())
    }

    fn confirms_on_accept(&self) -> bool {
        false
    }
}

/// An online-only confirmer that never reaches head office: the settle path under test does not
/// call it, and a spend made offline must be refused.
struct Unreachable;

impl Confirmer for Unreachable {
    fn confirm(&self, _entry: &Entry, _now: Millis) -> Confirmation {
        Confirmation::Unreachable
    }
}

fn debit(seq: u64, amount: i64) -> Entry {
    Entry::new(
        EntryId {
            node: node("branch-1"),
            seq,
        },
        card(),
        EntryKind::Debit,
        amount,
        Millis(1),
    )
    .expect("entry")
}

fn batch(entry: &Entry) -> Batch {
    Batch {
        node: node("branch-1"),
        entries: vec![entry.clone()],
        complete: false,
    }
}

fn contains(hub: &Hub, entry: &Entry) -> bool {
    hub.ledger(&card())
        .is_some_and(|ledger| ledger.contains(&entry.id))
}

#[test]
fn an_offered_entry_that_would_now_pass_is_queued_not_applied() {
    let mut hub = Hub::new(node("HO"));
    hub.open_balance(card(), 100, Millis(0)).expect("open");
    let policy = OnlineOnly::new(Unreachable);
    let entry = debit(1, 500);
    let mut sink = Buffering::default();

    // 1. The balance does not cover it: refused as insufficient, and only *offered* to the sink.
    let first = policy
        .settle(&mut hub, &batch(&entry), Millis(10), &mut sink)
        .expect("first delivery");
    assert!(first.applied.is_empty());
    assert_eq!(first.quarantined.len(), 1);
    assert_eq!(sink.offered.len(), 1);
    assert_eq!(hub.mark(&entry.id), Some(Mark::Offered));
    assert_eq!(hub.offered_unconfirmed(), vec![entry.id.clone()]);
    assert!(!hub.is_quarantined(&entry.id));

    // 2. Head office's state changes: a top-up makes the balance cover the entry.
    hub.credit(&card(), 1_000, Millis(20)).expect("credit");
    assert_eq!(hub.balance(&card()), Some(1_100));

    // 3. The sender redelivers the same entry, unchanged.
    let again = policy
        .settle(&mut hub, &batch(&entry), Millis(30), &mut sink)
        .expect("redelivery");

    // The entry whose refusal is still on its way to the queue is not applied as well: it is
    // completed into the queue, with the answer head office gave it the first time.
    assert!(
        !contains(&hub, &entry),
        "an offered entry is never applied on redelivery"
    );
    assert_eq!(hub.balance(&card()), Some(1_100), "the ledger is untouched");
    assert!(again.applied.is_empty());
    assert_eq!(
        again.quarantined,
        vec![(entry.id.clone(), RejectReason::Insufficient)]
    );
    assert_eq!(
        sink.offered,
        vec![
            (entry.id.clone(), RejectReason::Insufficient),
            (entry.id.clone(), RejectReason::Insufficient)
        ],
        "offered again with the same reason"
    );
    assert_eq!(hub.mark(&entry.id), Some(Mark::Offered));

    // 4. The queue write completes. Now the id is queued, and a further redelivery is a repeat:
    // still never applied, and never queued twice.
    assert!(hub.confirm_held(&entry.id));
    let repeat = policy
        .settle(&mut hub, &batch(&entry), Millis(40), &mut sink)
        .expect("repeat");
    assert_eq!(repeat.duplicates, vec![entry.id.clone()]);
    assert!(repeat.quarantined.is_empty());
    assert_eq!(sink.offered.len(), 2, "the sink is not offered it again");
    assert!(hub.is_quarantined(&entry.id));
    assert!(
        !contains(&hub, &entry),
        "an id is either applied or queued, never both"
    );
}

#[test]
fn a_queued_entry_is_not_applied_even_when_a_later_batch_covers_it() {
    // The same defect at the other end of the sink's life: the entry is *held* by a sink that
    // confirms on accept (a durable queue), and a much later delivery — after the balance has
    // grown tenfold — must still be a repeat rather than a spend.
    let mut hub = Hub::new(node("HO"));
    hub.open_balance(card(), 100, Millis(0)).expect("open");
    let policy = OnlineOnly::new(Unreachable);
    let entry = debit(9, 500);
    let mut sink = VecQuarantine::default();

    policy
        .settle(&mut hub, &batch(&entry), Millis(10), &mut sink)
        .expect("first delivery");
    assert!(hub.is_quarantined(&entry.id));

    hub.credit(&card(), 10_000, Millis(20)).expect("credit");
    let again = policy
        .settle(&mut hub, &batch(&entry), Millis(30), &mut sink)
        .expect("redelivery");

    assert_eq!(again.duplicates, vec![entry.id.clone()]);
    assert!(!contains(&hub, &entry));
    assert_eq!(sink.held.len(), 1, "one quarantine copy, ever");
    assert_eq!(hub.balance(&card()), Some(10_100));
}

#[test]
fn an_entry_head_office_never_offered_is_still_applied_normally() {
    // The guard must not turn into a blanket refusal: an entry with no mark at all is judged on
    // its merits and applied once the balance covers it.
    let mut hub = Hub::new(node("HO"));
    hub.open_balance(card(), 1_000, Millis(0)).expect("open");
    let policy = OnlineOnly::new(Unreachable);
    let entry = debit(7, 100);
    let mut sink = VecQuarantine::default();

    let reply = policy
        .settle(&mut hub, &batch(&entry), Millis(10), &mut sink)
        .expect("delivery");

    assert_eq!(reply.applied, vec![entry.id.clone()]);
    assert_eq!(hub.mark(&entry.id), None, "no refusal, no mark");
    assert_eq!(hub.balance(&card()), Some(900));
    assert!(sink.held.is_empty());
}
