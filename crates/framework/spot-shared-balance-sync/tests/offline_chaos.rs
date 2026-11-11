#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The WP-EDG-09.T1 offline chaos, re-run through [`receive_batch`].
//!
//! The first task proved each policy's bound when a node calls `settle` directly. Here every
//! delivery — progress reports, settlements, lost replies and redeliveries — goes through the
//! authenticated entry point, so the bound is shown for the shape the sync layer really uses:
//! every batch is authenticated against the transport identity and the node registry, and every
//! refused entry would go to the quarantine queue rather than vanish.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use spot_offline_identity::{MemoryAuditSink, NodeId as RegistryNodeId, NodeRegistry, TenantId};
use spot_shared_balance::{
    AcceptAndReconcile, AllocatedQuota, BalanceId, BalancePolicy, Batch, Confirmation, Confirmer,
    Decision, DeclineReason, Entry, EntryId, EntryKind, Hub, Millis, NodeId, NodeView, OnlineOnly,
    VecQuarantine,
};
use spot_shared_balance_sync::{HeadOffice, NodeIdentity, receive_batch};

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

fn registry() -> NodeRegistry {
    let mut registry = NodeRegistry::new(TenantId::new("acme").expect("tenant"));
    for i in 0..NODES {
        registry.register(RegistryNodeId::new(&format!("branch-{i:02}")).expect("node"));
    }
    registry
}

/// Head office as the online-only policy sees it: the live link confirms a debit unless the node
/// is cut off.
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
        // The live link applies the debit atomically; accept-and-reconcile is the plainest
        // applier and its limits do not matter for one confirmed entry.
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
    audit: MemoryAuditSink,
    tally: Tally,
    nodes: Vec<NodeView>,
}

fn run(
    seed: u64,
    policy: &dyn BalancePolicy,
    hub: Rc<RefCell<Hub>>,
    offline: Rc<RefCell<BTreeSet<NodeId>>>,
    uses_quota: bool,
) -> Outcome {
    let mut rng = Rng(seed);
    let registry = registry();
    let mut audit = MemoryAuditSink::default();
    let mut nodes: Vec<NodeView> = (0..NODES).map(|i| NodeView::new(node_id(i))).collect();
    let mut sink = VecQuarantine::default();
    let mut tally = Tally::default();
    let mut now = Millis(0);

    for i in 0..CARDS {
        hub.borrow_mut()
            .open_balance(card_id(i), OPENING, now)
            .expect("open");
    }
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
                    off.insert(id.clone());
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
                        &registry,
                        &mut audit,
                        &mut sink,
                        now,
                        uses_quota,
                        &mut rng,
                    );
                }
            }
        }
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
            let identity = NodeIdentity::authenticated(node.node().clone());
            let outcome = {
                let mut hub = hub.borrow_mut();
                receive_batch(
                    &identity,
                    batch,
                    now,
                    &mut sink,
                    HeadOffice {
                        registry: &registry,
                        hub: &mut hub,
                        policy,
                        audit: &mut audit,
                    },
                )
                .expect("final settle")
            };
            node.apply_hub_reply(&outcome);
        }
    }
    let hub = Rc::try_unwrap(hub)
        .map(RefCell::into_inner)
        .unwrap_or_else(|rc| rc.borrow().clone());
    Outcome {
        hub,
        sink,
        audit,
        tally,
        nodes,
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

#[allow(clippy::too_many_arguments)]
fn sync(
    node: &mut NodeView,
    policy: &dyn BalancePolicy,
    hub: &Rc<RefCell<Hub>>,
    registry: &NodeRegistry,
    audit: &mut MemoryAuditSink,
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
    let identity = NodeIdentity::authenticated(node.node().clone());
    let times = if rng.chance(30) { 2 } else { 1 };
    let mut last = None;
    for _ in 0..times {
        let outcome = {
            let mut hub = hub.borrow_mut();
            receive_batch(
                &identity,
                batch.clone(),
                now,
                sink,
                HeadOffice {
                    registry,
                    hub: &mut hub,
                    policy,
                    audit,
                },
            )
            .expect("settle")
        };
        last = Some(outcome);
    }
    let reply_arrived = !rng.chance(20);
    if let (true, Some(outcome)) = (reply_arrived, last) {
        node.apply_hub_reply(&outcome);
    }
    join(node, hub, now, uses_quota, rng);
}

/// The invariants every policy must keep, whatever its bound.
fn check_conservation(out: &Outcome) {
    assert!(
        out.audit.records.is_empty(),
        "no batch was refused for identity reasons: {:?}",
        out.audit.records.first()
    );
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
                EntryKind::Debit => debit_total += entry.amount,
                EntryKind::Credit => credits += entry.amount,
                EntryKind::Adjustment(_) => adjustments += entry.delta(),
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
        let offline = Rc::new(RefCell::new(BTreeSet::new()));
        let hub = Rc::new(RefCell::new(Hub::new(NodeId::new("head-office"))));
        let policy = OnlineOnly::new(SimConfirmer {
            hub: Rc::clone(&hub),
            offline: Rc::clone(&offline),
        });
        let out = run(seed, &policy, hub, offline, false);

        assert_eq!(out.tally.approved_while_offline, 0, "seed {seed}");
        assert!(out.tally.declined_offline > 0, "the chaos cut nodes off");
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
        check_conservation(&out);
    }
}

#[test]
fn allocated_quota_never_overspends_and_loses_nothing() {
    for seed in SEEDS {
        let policy = AllocatedQuota;
        let out = run(
            seed,
            &policy,
            Rc::new(RefCell::new(Hub::new(NodeId::new("head-office")))),
            Rc::new(RefCell::new(BTreeSet::new())),
            true,
        );

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
        assert!(out.hub.stranded(Millis(u64::MAX / 2)).is_empty());
        for c in 0..CARDS {
            assert_eq!(out.hub.reserved(&card_id(c)), 0);
        }
        check_conservation(&out);
    }
}

#[test]
fn accept_and_reconcile_stays_within_cap_times_nodes() {
    for seed in SEEDS {
        let cap = 120;
        let policy = AcceptAndReconcile::new(cap, NODES as u32).expect("policy");
        let out = run(
            seed,
            &policy,
            Rc::new(RefCell::new(Hub::new(NodeId::new("head-office")))),
            Rc::new(RefCell::new(BTreeSet::new())),
            false,
        );

        let bound = policy.limits().max_overspend;
        assert_eq!(bound, cap * NODES as i64);
        let mut per_card = [0_i64; CARDS];
        for receivable in out.hub.receivables() {
            let idx = (0..CARDS)
                .find(|c| card_id(*c) == receivable.balance)
                .expect("card");
            per_card[idx] += receivable.amount;
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
        check_conservation(&out);
    }
}
