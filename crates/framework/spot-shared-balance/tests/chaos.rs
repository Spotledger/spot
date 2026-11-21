#![allow(clippy::expect_used)]
//! Seeded offline chaos: 50 nodes, random offline periods, concurrent spends of the same gift
//! cards, lost replies, redelivery and restarts. Each policy's stated bound must hold, the
//! ledger sums must add up, and no entry may be lost or applied twice.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use spot_shared_balance::{
    AcceptAndReconcile, AllocatedQuota, BalanceId, BalancePolicy, Batch, Confirmation, Confirmer,
    Decision, DeclineReason, Entry, EntryId, Hub, Millis, NodeId, NodeView, OnlineOnly,
    VecQuarantine,
};

const NODES: usize = 50;
const CARDS: usize = 4;
const OPENING: i64 = 1_000;
const STEPS: usize = 6_000;
const SEEDS: [u64; 5] = [1, 7, 42, 1_234_567, 0xDEAD_BEEF];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn node_id(i: usize) -> NodeId {
    NodeId::new(format!("branch-{i:02}"))
}

fn card_id(i: usize) -> BalanceId {
    BalanceId::new(format!("card-{i}"))
}

/// Head office as the online-only policy sees it: a debit applies atomically unless the node is
/// cut off. The debit is applied with a plain `AcceptAndReconcile` settle of one entry, which
/// appends it; the policy under test is `OnlineOnly`, which only decides.
struct SimConfirmer {
    hub: Rc<RefCell<Hub>>,
    offline: Rc<RefCell<BTreeSet<NodeId>>>,
}

impl Confirmer for SimConfirmer {
    fn confirm(&self, entry: &Entry, now: Millis) -> Confirmation {
        if self.offline.borrow().contains(&entry.id.node) {
            return Confirmation::Unreachable;
        }
        let mut hub = self.hub.borrow_mut();
        let Some(ledger) = hub.ledger(&entry.balance) else {
            return Confirmation::UnknownBalance;
        };
        if ledger.contains(&entry.id) {
            return Confirmation::Confirmed;
        }
        if hub.available(&entry.balance).unwrap_or(0) < entry.amount {
            return Confirmation::Insufficient;
        }
        let applier = AcceptAndReconcile::new(i64::MAX / 4, 1).expect("applier");
        let batch = Batch {
            node: entry.id.node.clone(),
            entries: vec![entry.clone()],
            complete: false,
        };
        let mut sink = VecQuarantine::default();
        applier
            .settle(&mut hub, &batch, now, &mut sink)
            .expect("confirmation applies");
        Confirmation::Confirmed
    }
}

#[derive(Default)]
struct Tally {
    approved: Vec<Entry>,
    approved_while_offline: usize,
    declined_offline: usize,
    credits_added: i64,
}

struct Outcome {
    hub: Hub,
    sink: VecQuarantine,
    tally: Tally,
    nodes: Vec<NodeView>,
    now: Millis,
}

fn run(
    seed: u64,
    policy: &dyn BalancePolicy,
    hub: Rc<RefCell<Hub>>,
    offline: Rc<RefCell<BTreeSet<NodeId>>>,
    uses_quota: bool,
) -> Outcome {
    let mut rng = Rng(seed);
    let mut nodes: Vec<NodeView> = (0..NODES).map(|i| NodeView::new(node_id(i))).collect();
    let mut sink = VecQuarantine::default();
    let mut tally = Tally::default();
    let mut now = Millis(0);

    for i in 0..CARDS {
        hub.borrow_mut()
            .open_balance(card_id(i), OPENING, now)
            .expect("open");
    }
    // Every branch starts with a sync: it learns the balances, and a quota if the policy uses one.
    for node in nodes.iter_mut() {
        join(node, &hub, now, uses_quota, &mut rng);
    }

    for _ in 0..STEPS {
        now = now.plus(1 + rng.below(40));
        let i = rng.below(NODES as u64) as usize;
        let id = node_id(i);
        match rng.below(100) {
            0..=54 => {
                let card = card_id(rng.below(CARDS as u64) as usize);
                let amount = 1 + rng.below(250) as i64;
                let is_offline = offline.borrow().contains(&id);
                match policy.authorise(&mut nodes[i], &card, amount, now) {
                    Decision::Approved(entry) => {
                        if is_offline && !policy.limits().offline_spend {
                            tally.approved_while_offline += 1;
                        }
                        tally.approved.push(entry);
                    }
                    Decision::Declined(DeclineReason::Offline) => tally.declined_offline += 1,
                    Decision::Declined(_) => {}
                }
            }
            55..=64 => {
                let mut off = offline.borrow_mut();
                if !off.remove(&id) {
                    off.insert(id);
                }
            }
            65..=69 => {
                // A restart: the node keeps only what it persisted (its whole view).
                nodes[i] = nodes[i].clone();
            }
            70..=71 => {
                let card = card_id(rng.below(CARDS as u64) as usize);
                let amount = 1 + rng.below(100) as i64;
                if hub.borrow_mut().credit(&card, amount, now).is_ok() {
                    tally.credits_added += amount;
                }
            }
            _ => {
                if !offline.borrow().contains(&id) {
                    sync(
                        &mut nodes[i],
                        policy,
                        &hub,
                        &mut sink,
                        now,
                        uses_quota,
                        &mut rng,
                    );
                }
            }
        }
        // Per step: every ledger's running total equals the sum of its entries.
        if rng.chance(2) {
            let hub = hub.borrow();
            for c in 0..CARDS {
                let ledger = hub.ledger(&card_id(c)).expect("ledger");
                assert_eq!(Some(ledger.balance()), ledger.recomputed_balance());
            }
        }
    }

    // Everyone comes back online and reports in completely, twice (the second is a redelivery).
    offline.borrow_mut().clear();
    for node in nodes.iter_mut() {
        for _ in 0..2 {
            let batch = node.settlement_batch();
            let outcome = {
                let mut hub = hub.borrow_mut();
                policy.settle(&mut hub, &batch, now, &mut sink)
            }
            .expect("final settle");
            node.apply_hub_reply(&outcome);
        }
    }
    let hub = Rc::try_unwrap(hub)
        .map(RefCell::into_inner)
        .unwrap_or_else(|rc| rc.borrow().clone());
    Outcome {
        hub,
        sink,
        tally,
        nodes,
        now,
    }
}

fn join(node: &mut NodeView, hub: &Rc<RefCell<Hub>>, now: Millis, uses_quota: bool, rng: &mut Rng) {
    let mut hub = hub.borrow_mut();
    for c in 0..CARDS {
        let card = card_id(c);
        node.learn_balance(&card, hub.balance(&card).unwrap_or(0), now);
        if uses_quota && node.quota(&card).is_none() {
            let want = 20 + rng.below(80) as i64;
            // Head office judges expiry on when it *received* an entry, not on the node's own
            // time, so an entry is only honoured when it arrives before the lease runs
            // out. This seed asserts that a node following the protocol is never refused, and a
            // node cannot report while it is cut off: the lease therefore outlasts the whole run,
            // and expiry itself is covered by `hub_time.rs` and `policies.rs`.
            if let Ok(grant) = hub.grant_quota(node.node(), &card, want, now.plus(1_000_000), now) {
                node.install_grant(&grant);
            }
        }
    }
}

fn sync(
    node: &mut NodeView,
    policy: &dyn BalancePolicy,
    hub: &Rc<RefCell<Hub>>,
    sink: &mut VecQuarantine,
    now: Millis,
    uses_quota: bool,
    rng: &mut Rng,
) {
    // Quota nodes settle completely (they stop spending the old quota), accept nodes report
    // progress. Either way a delivery may be sent twice, and the reply may be lost.
    let batch = if uses_quota {
        node.settlement_batch()
    } else {
        node.progress_batch()
    };
    let times = if rng.chance(30) { 2 } else { 1 };
    let mut last = None;
    for _ in 0..times {
        let outcome = {
            let mut hub = hub.borrow_mut();
            policy.settle(&mut hub, &batch, now, sink)
        }
        .expect("settle");
        last = Some(outcome);
    }
    let reply_arrived = !rng.chance(20);
    if let (true, Some(outcome)) = (reply_arrived, last) {
        node.apply_hub_reply(&outcome);
    }
    join(node, hub, now, uses_quota, rng);
}

/// The invariants every policy must keep, whatever its bound.
fn check_conservation(out: &Outcome, applied_debits_expected: Option<i64>) {
    let hub = &out.hub;
    let mut debit_total = 0_i64;
    let mut seen: BTreeSet<EntryId> = BTreeSet::new();
    let mut adjustments = 0_i64;
    let mut credits = 0_i64;
    for c in 0..CARDS {
        let ledger = hub.ledger(&card_id(c)).expect("ledger");
        assert_eq!(
            Some(ledger.balance()),
            ledger.recomputed_balance(),
            "running total equals the sum of entries"
        );
        for entry in ledger.entries() {
            assert!(
                seen.insert(entry.id.clone()),
                "entry {} appears once",
                entry.id
            );
            match entry.kind {
                spot_shared_balance::EntryKind::Debit => debit_total += entry.amount,
                spot_shared_balance::EntryKind::Credit => credits += entry.amount,
                spot_shared_balance::EntryKind::Adjustment(_) => adjustments += entry.delta(),
            }
        }
    }
    assert_eq!(
        credits,
        OPENING * CARDS as i64 + out.tally.credits_added,
        "credits are the openings and the top-ups"
    );
    assert_eq!(
        adjustments,
        hub.receivable_total(),
        "adjustments are the receivables"
    );

    // No entry lost, none duplicated: every approved debit is in a ledger or in quarantine,
    // exactly once.
    let quarantined: BTreeSet<EntryId> = out.sink.held.iter().map(|(e, _)| e.id.clone()).collect();
    assert_eq!(
        quarantined.len(),
        out.sink.held.len(),
        "nothing is quarantined twice"
    );
    let mut approved_total = 0_i64;
    for entry in &out.tally.approved {
        approved_total += entry.amount;
        let in_ledger = seen.contains(&entry.id);
        let in_quarantine = quarantined.contains(&entry.id);
        assert!(
            in_ledger ^ in_quarantine,
            "approved entry {} is in exactly one place",
            entry.id
        );
    }
    let quarantined_total: i64 = out.sink.held.iter().map(|(e, _)| e.amount).sum();
    assert_eq!(
        approved_total,
        debit_total + quarantined_total,
        "approved value equals applied plus quarantined value"
    );
    if let Some(expected) = applied_debits_expected {
        assert_eq!(debit_total, expected);
    }
    // Nodes have nothing left to report.
    for node in &out.nodes {
        assert!(
            node.pending().is_empty(),
            "{} reported everything",
            node.node()
        );
        for c in 0..CARDS {
            if let Some(local) = node.local_ledger(&card_id(c)) {
                assert_eq!(Some(local.balance()), local.recomputed_balance());
            }
        }
    }
}

#[test]
fn online_only_never_overspends_and_never_sells_offline() {
    for seed in SEEDS {
        let hub = Rc::new(RefCell::new(Hub::new(NodeId::new("HO"))));
        let offline = Rc::new(RefCell::new(BTreeSet::new()));
        let policy = OnlineOnly::new(SimConfirmer {
            hub: hub.clone(),
            offline: offline.clone(),
        });
        let out = run(seed, &policy, hub, offline, false);

        assert_eq!(out.tally.approved_while_offline, 0, "seed {seed}");
        assert!(
            out.tally.declined_offline > 0,
            "the chaos did cut nodes off"
        );
        assert_eq!(out.hub.receivable_total(), 0);
        assert!(
            out.sink.held.is_empty(),
            "every online-only debit was confirmed"
        );
        for c in 0..CARDS {
            assert!(
                out.hub.balance(&card_id(c)).unwrap_or(-1) >= 0,
                "seed {seed}"
            );
        }
        check_conservation(&out, None);
    }
}

#[test]
fn allocated_quota_never_overspends_and_loses_nothing() {
    for seed in SEEDS {
        let hub = Rc::new(RefCell::new(Hub::new(NodeId::new("HO"))));
        let offline = Rc::new(RefCell::new(BTreeSet::new()));
        let policy = AllocatedQuota;
        let out = run(seed, &policy, hub, offline, true);

        assert_eq!(out.hub.receivable_total(), 0);
        assert!(
            out.sink.held.is_empty(),
            "seed {seed}: a node that follows the protocol is never refused: {:?}",
            out.sink.held.first()
        );
        assert!(!out.tally.approved.is_empty());
        for c in 0..CARDS {
            let balance = out.hub.balance(&card_id(c)).unwrap_or(-1);
            assert!(balance >= 0, "seed {seed}: card {c} went to {balance}");
        }
        // Everything settled and every grant closed: nothing is stranded or reserved.
        assert!(out.hub.stranded(out.now.plus(10_000)).is_empty());
        for c in 0..CARDS {
            assert_eq!(out.hub.reserved(&card_id(c)), 0);
        }
        check_conservation(&out, None);
    }
}

#[test]
fn accept_and_reconcile_stays_within_cap_times_nodes() {
    for seed in SEEDS {
        let cap = 120;
        let hub = Rc::new(RefCell::new(Hub::new(NodeId::new("HO"))));
        let offline = Rc::new(RefCell::new(BTreeSet::new()));
        let policy = AcceptAndReconcile::new(cap, NODES as u32).expect("policy");
        let out = run(seed, &policy, hub, offline, false);

        let bound = policy.limits().max_overspend;
        assert_eq!(bound, cap * NODES as i64);
        // The bound is per balance: each card can be overspent by at most cap x nodes.
        let mut per_card = [0_i64; CARDS];
        for r in out.hub.receivables() {
            let idx = (0..CARDS).find(|c| card_id(*c) == r.balance).expect("card");
            per_card[idx] += r.amount;
        }
        for (c, total) in per_card.iter().enumerate() {
            assert!(
                *total <= bound,
                "seed {seed}: card {c} overspent by {total} > {bound}"
            );
        }
        assert!(
            out.hub.receivable_total() > 0 || seed != SEEDS[0],
            "the chaos is strong enough to overspend at least once on the first seed"
        );
        assert!(out.sink.held.is_empty());
        for c in 0..CARDS {
            assert!(out.hub.balance(&card_id(c)).unwrap_or(-1) >= 0);
        }
        check_conservation(&out, None);
    }
}
