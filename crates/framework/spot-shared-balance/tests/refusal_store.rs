#![allow(clippy::expect_used)]
//! Recorded security finding: head office's refusal marks survive a restart, and a durable store must pass the
//! contract suite.
//!
//! A mark that lives only in memory is forgotten by a restart: an entry the queue holds comes back
//! unknown (and could be applied a second time), and an entry only *offered* to the queue is
//! forgotten entirely (so nothing offers it again and the refusal is lost). The store port fixes
//! what a durable implementation must guarantee — mark and reason in one transaction, all records
//! or none — and this file runs that contract against the in-memory store plus the restart itself.

use spot_shared_balance::{
    AllocatedQuota, BalanceId, BalancePolicy, Batch, Entry, EntryId, EntryKind, Hub, Mark,
    MemoryRefusalStore, Millis, NodeId, QuarantineSink, RefusalRecord, RefusalStore, RejectReason,
    SinkError, VecQuarantine, testing::assert_refusal_store_contract,
};

fn node(name: &str) -> NodeId {
    NodeId::new(name)
}

fn card() -> BalanceId {
    BalanceId::new("gift-card-1")
}

/// A sink that buffers: the hub's mark stays *offered* until a queue write is confirmed.
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

/// An entry for a balance head office does not hold, so it is always refused.
fn refused(seq: u64) -> Entry {
    Entry::new(
        EntryId {
            node: node("branch-1"),
            seq,
        },
        BalanceId::new("no-such-card"),
        EntryKind::Debit,
        5,
        Millis(seq),
    )
    .expect("entry")
}

fn batch(entries: Vec<Entry>) -> Batch {
    Batch {
        node: node("branch-1"),
        entries,
        complete: false,
    }
}

#[test]
fn the_in_memory_store_keeps_the_contract() {
    assert_refusal_store_contract(MemoryRefusalStore::new);
}

#[test]
fn a_restart_keeps_both_marks_and_re_offers_what_the_queue_never_confirmed() {
    let mut hub = Hub::new(node("HO"));
    hub.open_balance(card(), 100, Millis(0)).expect("open");
    let policy = AllocatedQuota;
    let offered = refused(1);
    let held = refused(2);
    let mut buffered = Buffering::default();
    let mut queue = VecQuarantine::default();

    // One entry stays only offered (the queue write is still pending), one is confirmed held.
    policy
        .settle(
            &mut hub,
            &batch(vec![offered.clone()]),
            Millis(5),
            &mut buffered,
        )
        .expect("offered");
    policy
        .settle(&mut hub, &batch(vec![held.clone()]), Millis(6), &mut queue)
        .expect("held");
    assert_eq!(hub.mark(&offered.id), Some(Mark::Offered));
    assert_eq!(hub.mark(&held.id), Some(Mark::Held));

    let mut store = MemoryRefusalStore::new();
    hub.persist_refusals(&mut store).expect("the marks persist");
    let persisted = store.load().expect("the store loads");
    assert_eq!(
        persisted,
        vec![
            RefusalRecord {
                id: offered.id.clone(),
                mark: Mark::Offered,
                reason: RejectReason::UnknownBalance,
            },
            RefusalRecord {
                id: held.id.clone(),
                mark: Mark::Held,
                reason: RejectReason::UnknownBalance,
            },
        ],
        "the mark and the reason it was refused with"
    );

    // A restart: a new hub, with nothing but the store's marks.
    let mut restarted = Hub::new(node("HO"));
    restarted
        .restore_refusals(&store)
        .expect("the marks come back");
    assert_eq!(
        restarted.offered_unconfirmed(),
        vec![offered.id.clone()],
        "the entry the queue never confirmed is re-offered on start"
    );
    assert!(restarted.is_quarantined(&held.id));

    // The held entry's redelivery is a repeat: a restart cannot make it applicable again.
    let mut queue = VecQuarantine::default();
    let repeat = policy
        .settle(
            &mut restarted,
            &batch(vec![held.clone()]),
            Millis(20),
            &mut queue,
        )
        .expect("repeat");
    assert_eq!(repeat.duplicates, vec![held.id.clone()]);
    assert!(queue.held.is_empty(), "the queue is not offered it again");

    // The offered entry's redelivery completes it into the queue, with its original reason.
    let mut second_queue = VecQuarantine::default();
    let again = policy
        .settle(
            &mut restarted,
            &batch(vec![offered.clone()]),
            Millis(21),
            &mut second_queue,
        )
        .expect("re-offered");
    assert_eq!(
        again.quarantined,
        vec![(offered.id.clone(), RejectReason::UnknownBalance)]
    );
    assert!(restarted.is_quarantined(&offered.id));
}

#[test]
fn a_hub_with_marks_already_cannot_be_restored_over() {
    // Fail closed: a restart that would mix two stores' marks is refused, not merged.
    let mut hub = Hub::new(node("HO"));
    let policy = AllocatedQuota;
    let mut sink = VecQuarantine::default();
    let entry = refused(3);
    policy
        .settle(&mut hub, &batch(vec![entry]), Millis(5), &mut sink)
        .expect("held");
    let store = MemoryRefusalStore::new();
    assert!(hub.restore_refusals(&store).is_err());
    assert!(hub.is_quarantined(&refused(3).id), "the hub is unchanged");
}

#[test]
fn a_store_that_fails_leaves_the_hubs_marks_where_they_were() {
    let mut hub = Hub::new(node("HO"));
    let policy = AllocatedQuota;
    let mut sink = VecQuarantine::default();
    let entry = refused(4);
    policy
        .settle(&mut hub, &batch(vec![entry.clone()]), Millis(5), &mut sink)
        .expect("held");

    let mut store = MemoryRefusalStore::failing(1);
    assert!(hub.persist_refusals(&mut store).is_err());
    assert_eq!(store.load().expect("load").len(), 0, "nothing was written");
    assert!(hub.is_quarantined(&entry.id), "the hub keeps its marks");
    // The retry lands.
    hub.persist_refusals(&mut store)
        .expect("the retry persists");
    assert_eq!(store.len(), 1);
}
