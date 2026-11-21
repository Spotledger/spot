#![allow(clippy::expect_used)]
//! The interleaving property: whatever order deliveries, replies,
//! offers and redeliveries arrive in, an entry id is applied at most once and is never both applied
//! and queued, and a node's spend never exceeds its quota plus the policy's stated bound (zero, for
//! the allocated-quota policy).
//!
//! Two senders share the card, which is what makes the interleaving interesting:
//!
//! - `branch-00` follows the protocol: it spends inside its quota, delivers, occasionally loses
//!   the reply or redelivers, and applies what head office answers.
//! - `branch-rogue` has no quota, so everything it sends is refused (`NoGrant`). Part-way through
//!   the run head office grants it one, which is exactly the situation a recorded security finding lived in: a refusal
//!   that was only *offered* to the quarantine sink now passes the policy, and re-running the
//!   policy on the redelivery used to apply an entry whose quarantine copy was still on its way.
//!
//! Deterministic (a small LCG, so a failing seed reproduces), no dependencies.

use std::collections::{BTreeMap, BTreeSet};

use spot_shared_balance::{
    AllocatedQuota, BalanceId, BalancePolicy, Batch, Decision, Entry, EntryId, EntryKind, Hub,
    Millis, NodeId, NodeView, RejectReason, VecQuarantine,
};

const STEPS: usize = 500;
const GRANT: i64 = 100;
const ROGUE_GRANT: i64 = 40;
const SEEDS: [u64; 8] = [1, 3, 7, 42, 99, 1_234_567, 0xDEAD_BEEF, 0x5EED];

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn card() -> BalanceId {
    BalanceId::new("gift-card-1")
}

fn honest() -> NodeId {
    NodeId::new("branch-00")
}

fn rogue() -> NodeId {
    NodeId::new("branch-rogue")
}

fn batch(who: &NodeId, entries: Vec<Entry>) -> Batch {
    Batch {
        node: who.clone(),
        entries,
        complete: false,
    }
}

/// What one seed's run left behind.
struct Run {
    hub: Hub,
    /// Every entry a sender delivered, so the check can account for each one.
    sent: Vec<Entry>,
    /// What the quarantine sink ended up holding.
    queued: Vec<(Entry, RejectReason)>,
}

fn run(seed: u64) -> Run {
    let mut rng = Lcg(seed | 1);
    let mut hub = Hub::new(NodeId::new("HO"));
    hub.open_balance(card(), 600, Millis(0)).expect("open");
    let policy = AllocatedQuota;
    let mut view = NodeView::new(honest());
    let mut now = Millis(0);
    let mut sent: Vec<Entry> = Vec::new();
    let mut sink = VecQuarantine::default();

    view.learn_balance(&card(), 600, now);
    let grant = hub
        .grant_quota(&honest(), &card(), GRANT, now.plus(10_000_000), now)
        .expect("grant");
    view.install_grant(&grant);

    let mut rogue_seq = 1_u64;
    let mut rogue_granted = false;
    let mut rogue_batch: Vec<Entry> = Vec::new();

    for _ in 0..STEPS {
        now = now.plus(1 + rng.below(3));
        match rng.below(100) {
            // The honest node spends inside its quota.
            0..=34 => {
                let amount = 1 + rng.below(30) as i64;
                if let Decision::Approved(entry) = policy.authorise(&mut view, &card(), amount, now)
                {
                    sent.push(entry);
                }
            }
            // It delivers: sometimes twice, and the reply may be lost.
            35..=59 => {
                let delivery = view.progress_batch();
                let times = if rng.chance(30) { 2 } else { 1 };
                let mut last = None;
                for _ in 0..times {
                    let reply = policy
                        .settle(&mut hub, &delivery, now, &mut sink)
                        .expect("settle");
                    last = Some(reply);
                }
                if let (true, Some(reply)) = (!rng.chance(25), last) {
                    view.apply_hub_reply(&reply);
                }
            }
            // The rogue sends a debit it has no quota for, and keeps redelivering its backlog.
            60..=79 => {
                let amount = 1 + rng.below(20) as i64;
                if let Ok(entry) = Entry::new(
                    EntryId {
                        node: rogue(),
                        seq: rogue_seq,
                    },
                    card(),
                    EntryKind::Debit,
                    amount,
                    now,
                ) {
                    rogue_seq = rogue_seq.saturating_add(1);
                    rogue_batch.push(entry.clone());
                    sent.push(entry);
                }
                policy
                    .settle(
                        &mut hub,
                        &batch(&rogue(), rogue_batch.clone()),
                        now,
                        &mut sink,
                    )
                    .expect("rogue delivery");
            }
            // Head office's state moves: the rogue is granted a quota (so its refused backlog
            // would now pass) and the card is topped up. This is the recorded security finding trigger.
            80..=89 => {
                if !rogue_granted && rng.chance(60) {
                    rogue_granted = hub
                        .grant_quota(&rogue(), &card(), ROGUE_GRANT, now.plus(10_000_000), now)
                        .is_ok();
                }
                hub.credit(&card(), 1 + rng.below(50) as i64, now)
                    .expect("credit");
            }
            // The sink fails (entries stay offered and deferred), or a queue write lands.
            _ => {
                if rng.chance(50) {
                    sink.fail_next = rng.below(3) as u32;
                } else {
                    for id in hub.offered_unconfirmed() {
                        if rng.chance(50) {
                            hub.confirm_held(&id);
                        }
                    }
                }
            }
        }

        // The invariants hold at every step, not only at the end.
        assert_no_double_place(seed, &hub, &sink.held);
    }

    // Everyone reports in completely: the lease closes and the backlog is delivered.
    let settlement = view.settlement_batch();
    let reply = policy
        .settle(&mut hub, &settlement, now, &mut sink)
        .expect("final settle");
    view.apply_hub_reply(&reply);
    policy
        .settle(&mut hub, &batch(&rogue(), rogue_batch), now, &mut sink)
        .expect("final rogue delivery");
    for id in hub.offered_unconfirmed() {
        hub.confirm_held(&id);
    }

    Run {
        hub,
        sent,
        queued: sink.held,
    }
}

/// No entry id is on the ledger twice, none is queued twice, and none is both applied and queued.
fn assert_no_double_place(seed: u64, hub: &Hub, queued: &[(Entry, RejectReason)]) {
    let mut applied: BTreeSet<&EntryId> = BTreeSet::new();
    if let Some(ledger) = hub.ledger(&card()) {
        for entry in ledger.entries() {
            assert!(
                applied.insert(&entry.id),
                "seed {seed}: entry {} is on the ledger twice",
                entry.id
            );
        }
    }
    let mut in_queue: BTreeSet<&EntryId> = BTreeSet::new();
    for (entry, _) in queued {
        assert!(
            in_queue.insert(&entry.id),
            "seed {seed}: entry {} is queued twice",
            entry.id
        );
    }
    for id in &in_queue {
        assert!(
            !applied.contains(id),
            "seed {seed}: entry {id} is both applied and queued"
        );
    }
    for id in hub.offered_unconfirmed() {
        assert!(
            !applied.contains(&id),
            "seed {seed}: entry {id} is offered to quarantine and applied"
        );
    }
}

fn check(seed: u64, run: &Run) {
    let Run { hub, sent, queued } = run;
    assert_no_double_place(seed, hub, queued);

    let ledger = hub.ledger(&card()).expect("the ledger exists");
    assert_eq!(Some(ledger.balance()), ledger.recomputed_balance());
    let applied: BTreeSet<&EntryId> = ledger.entries().iter().map(|entry| &entry.id).collect();
    let in_queue: BTreeSet<&EntryId> = queued.iter().map(|(entry, _)| &entry.id).collect();

    // Every entry a sender delivered is accounted for exactly once: on the ledger, in the queue,
    // or still marked as refused and waiting for the queue (offered) after a failed sink.
    for entry in sent {
        let places =
            usize::from(applied.contains(&entry.id)) + usize::from(in_queue.contains(&entry.id));
        if places == 0 {
            assert!(
                hub.mark(&entry.id).is_some(),
                "seed {seed}: entry {} was delivered and is nowhere",
                entry.id
            );
        }
        assert!(
            places <= 1,
            "seed {seed}: entry {} is in {places} places",
            entry.id
        );
    }

    // The quota bound: what head office applied for a node never exceeds the quota it lent it.
    let mut spent: BTreeMap<&NodeId, i64> = BTreeMap::new();
    for entry in ledger.entries() {
        if entry.is_debit() {
            let total = spent.entry(&entry.id.node).or_insert(0);
            *total = total.saturating_add(entry.amount);
        }
    }
    for (node, total) in spent {
        let bound = if node == &rogue() { ROGUE_GRANT } else { GRANT };
        assert!(
            total <= bound,
            "seed {seed}: node {node} spent {total} against a quota of {bound}"
        );
    }
    assert_eq!(
        AllocatedQuota.limits().max_overspend,
        0,
        "no overspend at all"
    );
}

#[test]
fn any_interleaving_applies_an_id_once_and_never_both_applied_and_queued() {
    for seed in SEEDS {
        let run = run(seed);
        check(seed, &run);
    }
}
