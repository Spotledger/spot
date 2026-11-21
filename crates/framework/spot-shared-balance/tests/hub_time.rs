#![allow(clippy::expect_used)]
//! Recorded security finding: quota expiry is judged on head office's own receive time, never on the node's.
//!
//! A node's `at` is that node's clock, and a node that wants to spend an expired quota only has
//! to claim an earlier time. Head office therefore stamps every entry with the moment it first
//! saw it, and judges expiry on that. The node's time stays on the entry as information.

use spot_shared_balance::{
    AllocatedQuota, BalanceId, BalancePolicy, Batch, Entry, EntryId, EntryKind, Hub, Millis,
    NodeId, RejectReason, VecQuarantine,
};

fn node(name: &str) -> NodeId {
    NodeId::new(name)
}

fn card() -> BalanceId {
    BalanceId::new("gift-card-1")
}

/// Head office holding 1,000 on the card, with a 100 quota for `branch-1` that expires at
/// `expires_at`.
fn hub_with_quota(expires_at: Millis) -> Hub {
    let mut hub = Hub::new(node("HO"));
    hub.open_balance(card(), 1_000, Millis(0)).expect("open");
    hub.grant_quota(&node("branch-1"), &card(), 100, expires_at, Millis(0))
        .expect("grant");
    hub
}

fn debit(seq: u64, amount: i64, at: Millis) -> Entry {
    Entry::new(
        EntryId {
            node: node("branch-1"),
            seq,
        },
        card(),
        EntryKind::Debit,
        amount,
        at,
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
fn a_backdated_entry_is_judged_by_head_offices_receive_time() {
    // The quota expired at 500. The entry claims it was written at 1, long before the expiry —
    // but head office first sees it at 700, which is after the quota closed.
    let mut hub = hub_with_quota(Millis(500));
    let entry = debit(1, 40, Millis(1));
    let mut sink = VecQuarantine::default();

    let reply = AllocatedQuota
        .settle(
            &mut hub,
            &batch(vec![entry.clone()]),
            Millis(700),
            &mut sink,
        )
        .expect("settle");

    assert!(
        reply.applied.is_empty(),
        "a backdated entry cannot spend an expired quota"
    );
    assert_eq!(sink.held.len(), 1, "it is quarantined, not dropped");
    assert_eq!(sink.held[0].1, RejectReason::AfterExpiry);
    assert_eq!(hub.balance(&card()), Some(1_000), "nothing was spent");
    assert_eq!(
        hub.received_at(&entry.id),
        Some(Millis(700)),
        "head office recorded when it saw the entry"
    );
}

#[test]
fn the_nodes_own_time_is_information_only() {
    // The mirror image: the node's clock says the entry was written after the expiry, but head
    // office saw it while the quota was still open. Expiry is head office's to judge, so the
    // entry is applied — and the node's time is still on it, unchanged, as information.
    let mut hub = hub_with_quota(Millis(500));
    let entry = debit(2, 40, Millis(900));
    let mut sink = VecQuarantine::default();

    let reply = AllocatedQuota
        .settle(
            &mut hub,
            &batch(vec![entry.clone()]),
            Millis(100),
            &mut sink,
        )
        .expect("settle");

    assert_eq!(reply.applied, vec![entry.id.clone()]);
    assert_eq!(hub.received_at(&entry.id), Some(Millis(100)));
    assert_eq!(hub.balance(&card()), Some(960));
    let held = hub
        .ledger(&card())
        .and_then(|ledger| ledger.entry(&entry.id).cloned())
        .expect("the entry is on the ledger");
    assert_eq!(held.at, Millis(900), "the node's time is kept as it was");
}

#[test]
fn the_receive_time_is_the_first_one_head_office_saw() {
    // A redelivery hours later does not move the receive time: the first sighting is what the
    // expiry rule is judged on, so a node cannot revive an entry by sending it again.
    let mut hub = hub_with_quota(Millis(500));
    let entry = debit(3, 10, Millis(1));
    let mut sink = VecQuarantine::default();

    AllocatedQuota
        .settle(
            &mut hub,
            &batch(vec![entry.clone()]),
            Millis(700),
            &mut sink,
        )
        .expect("first delivery");
    assert_eq!(hub.received_at(&entry.id), Some(Millis(700)));

    let again = AllocatedQuota
        .settle(
            &mut hub,
            &batch(vec![entry.clone()]),
            Millis(900),
            &mut sink,
        )
        .expect("redelivery");
    assert_eq!(
        hub.received_at(&entry.id),
        Some(Millis(700)),
        "the first receive time stands"
    );
    assert_eq!(
        again.duplicates,
        vec![entry.id],
        "the entry is held already"
    );
    assert_eq!(hub.balance(&card()), Some(1_000));
}
