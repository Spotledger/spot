#![allow(clippy::expect_used)]
//! Behaviour of each policy on small, readable scenarios.

use std::cell::RefCell;
use std::rc::Rc;

use spot_shared_balance::{
    AcceptAndReconcile, AllocatedQuota, BalanceError, BalanceId, BalancePolicy, Confirmation,
    Confirmer, Decision, DeclineReason, Entry, EntryId, EntryKind, Hub, Ledger, Millis, NodeId,
    NodeView, OnlineOnly, PROVISIONAL_DEFAULT, PolicyKind, PolicyTable, RejectReason,
    VecQuarantine,
};

fn node(name: &str) -> NodeId {
    NodeId::new(name)
}

fn card() -> BalanceId {
    BalanceId::new("gift-card-1")
}

fn hub_with(opening: i64) -> Hub {
    let mut hub = Hub::new(node("HO"));
    hub.open_balance(card(), opening, Millis(0)).expect("open");
    hub
}

struct LiveHub {
    hub: Rc<RefCell<Hub>>,
    reachable: Rc<RefCell<bool>>,
}

impl Confirmer for LiveHub {
    fn confirm(&self, entry: &Entry, _now: Millis) -> Confirmation {
        if !*self.reachable.borrow() {
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
        hub.apply_for_test(entry);
        Confirmation::Confirmed
    }
}

/// Test-only access to the hub ledger through the public surface: credit/debit via settle.
trait HubExt {
    fn apply_for_test(&mut self, entry: &Entry);
}

impl HubExt for Hub {
    fn apply_for_test(&mut self, entry: &Entry) {
        // A confirmation is head office applying the debit itself, which is exactly an
        // account-policy settle of one entry; the quota-free settle path does it.
        let policy = AcceptAndReconcile::new(i64::MAX / 4, 1).expect("policy");
        let batch = spot_shared_balance::Batch {
            node: entry.id.node.clone(),
            entries: vec![entry.clone()],
            complete: false,
        };
        let mut sink = VecQuarantine::default();
        policy
            .settle(self, &batch, entry.at, &mut sink)
            .expect("confirm applies");
    }
}

#[test]
fn online_only_approves_when_reachable_and_covered() {
    let hub = Rc::new(RefCell::new(hub_with(1000)));
    let reachable = Rc::new(RefCell::new(true));
    let policy = OnlineOnly::new(LiveHub {
        hub: hub.clone(),
        reachable: reachable.clone(),
    });
    let mut view = NodeView::new(node("branch-1"));

    let first = policy.authorise(&mut view, &card(), 600, Millis(10));
    assert!(first.is_approved());
    assert_eq!(hub.borrow().balance(&card()), Some(400));

    let second = policy.authorise(&mut view, &card(), 600, Millis(20));
    assert_eq!(second, Decision::Declined(DeclineReason::Insufficient));
    assert_eq!(hub.borrow().balance(&card()), Some(400));
    assert!(view.pending().is_empty(), "nothing left to report");
}

#[test]
fn online_only_declines_every_spend_offline() {
    let hub = Rc::new(RefCell::new(hub_with(1000)));
    let reachable = Rc::new(RefCell::new(false));
    let policy = OnlineOnly::new(LiveHub {
        hub: hub.clone(),
        reachable,
    });
    let mut view = NodeView::new(node("branch-1"));
    assert_eq!(
        policy.authorise(&mut view, &card(), 1, Millis(1)),
        Decision::Declined(DeclineReason::Offline)
    );
    assert_eq!(hub.borrow().balance(&card()), Some(1000));
    assert_eq!(policy.limits().max_overspend, 0);
    assert!(!policy.limits().offline_spend);
}

#[test]
fn invalid_amounts_are_declined_not_recorded() {
    let hub = Rc::new(RefCell::new(hub_with(1000)));
    let policy = OnlineOnly::new(LiveHub {
        hub,
        reachable: Rc::new(RefCell::new(true)),
    });
    let mut view = NodeView::new(node("b"));
    for amount in [0, -5] {
        assert_eq!(
            policy.authorise(&mut view, &card(), amount, Millis(1)),
            Decision::Declined(DeclineReason::InvalidAmount)
        );
    }
}

#[test]
fn online_only_quarantines_an_entry_that_bypassed_confirmation() {
    let mut hub = hub_with(100);
    let policy = OnlineOnly::new(LiveHub {
        hub: Rc::new(RefCell::new(hub_with(100))),
        reachable: Rc::new(RefCell::new(true)),
    });
    let entry = Entry::new(
        EntryId {
            node: node("rogue"),
            seq: 1,
        },
        card(),
        EntryKind::Debit,
        500,
        Millis(5),
    )
    .expect("entry");
    let batch = spot_shared_balance::Batch {
        node: node("rogue"),
        entries: vec![entry.clone()],
        complete: false,
    };
    let mut sink = VecQuarantine::default();
    let outcome = policy
        .settle(&mut hub, &batch, Millis(9), &mut sink)
        .expect("settle");
    assert_eq!(outcome.quarantined.len(), 1);
    assert_eq!(sink.held.len(), 1);
    assert_eq!(hub.balance(&card()), Some(100), "balance untouched");

    // The same delivery again is a repeat: still one item in quarantine.
    let again = policy
        .settle(&mut hub, &batch, Millis(10), &mut sink)
        .expect("settle");
    assert_eq!(again.duplicates, vec![entry.id]);
    assert_eq!(sink.held.len(), 1);
}

#[test]
fn quota_reserves_value_and_lets_a_node_spend_offline() {
    let mut hub = hub_with(1000);
    let policy = AllocatedQuota;
    let mut view = NodeView::new(node("branch-1"));
    let grant = hub
        .grant_quota(&node("branch-1"), &card(), 300, Millis(1000), Millis(0))
        .expect("grant");
    view.install_grant(&grant);
    assert_eq!(hub.available(&card()), Some(700));

    assert!(
        policy
            .authorise(&mut view, &card(), 200, Millis(10))
            .is_approved()
    );
    assert_eq!(
        policy.authorise(&mut view, &card(), 101, Millis(20)),
        Decision::Declined(DeclineReason::QuotaExhausted)
    );
    assert!(
        policy
            .authorise(&mut view, &card(), 100, Millis(30))
            .is_approved()
    );
    assert_eq!(view.quota_remaining(&card()), Some(0));
}

#[test]
fn quota_declines_without_a_grant_and_after_expiry() {
    let mut hub = hub_with(1000);
    let policy = AllocatedQuota;
    let mut view = NodeView::new(node("b"));
    assert_eq!(
        policy.authorise(&mut view, &card(), 10, Millis(1)),
        Decision::Declined(DeclineReason::NoQuota)
    );
    let grant = hub
        .grant_quota(&node("b"), &card(), 100, Millis(50), Millis(0))
        .expect("grant");
    view.install_grant(&grant);
    assert_eq!(
        policy.authorise(&mut view, &card(), 10, Millis(50)),
        Decision::Declined(DeclineReason::QuotaExpired)
    );
}

#[test]
fn quota_settlement_returns_the_unused_value() {
    let mut hub = hub_with(1000);
    let policy = AllocatedQuota;
    let mut view = NodeView::new(node("b"));
    let grant = hub
        .grant_quota(&node("b"), &card(), 400, Millis(1000), Millis(0))
        .expect("grant");
    view.install_grant(&grant);
    assert!(
        policy
            .authorise(&mut view, &card(), 150, Millis(5))
            .is_approved()
    );

    let batch = view.settlement_batch();
    let mut sink = VecQuarantine::default();
    let outcome = policy
        .settle(&mut hub, &batch, Millis(100), &mut sink)
        .expect("settle");
    view.apply_hub_reply(&outcome);

    assert_eq!(outcome.applied.len(), 1);
    assert_eq!(outcome.returned, 250);
    assert_eq!(hub.balance(&card()), Some(850));
    assert_eq!(hub.available(&card()), Some(850), "nothing stays reserved");
    assert!(view.pending().is_empty());
    assert!(hub.grant(&node("b"), &card()).is_none());
}

#[test]
fn quota_grant_cannot_exceed_what_is_available_or_double_up() {
    let mut hub = hub_with(500);
    hub.grant_quota(&node("a"), &card(), 400, Millis(10), Millis(0))
        .expect("grant");
    let too_much = hub.grant_quota(&node("b"), &card(), 200, Millis(10), Millis(0));
    assert!(matches!(
        too_much,
        Err(BalanceError::InsufficientAvailable { .. })
    ));
    let twice = hub.grant_quota(&node("a"), &card(), 10, Millis(10), Millis(0));
    assert!(matches!(twice, Err(BalanceError::GrantExists { .. })));
}

#[test]
fn an_unsettled_expired_quota_is_reported_as_stranded() {
    let mut hub = hub_with(1000);
    hub.grant_quota(&node("lost-node"), &card(), 300, Millis(100), Millis(0))
        .expect("grant");
    assert!(hub.stranded(Millis(99)).is_empty());
    let stranded = hub.stranded(Millis(100));
    assert_eq!(stranded.len(), 1);
    assert_eq!(stranded[0].amount, 300);
    assert_eq!(stranded[0].node, node("lost-node"));
    assert_eq!(hub.available(&card()), Some(700), "still reserved");
}

#[test]
fn quota_quarantines_an_entry_past_the_grant_or_after_expiry() {
    let mut hub = hub_with(1000);
    let policy = AllocatedQuota;
    hub.grant_quota(&node("n"), &card(), 100, Millis(500), Millis(0))
        .expect("grant");
    let mk = |seq: u64, amount: i64, at: u64| {
        Entry::new(
            EntryId {
                node: node("n"),
                seq,
            },
            card(),
            EntryKind::Debit,
            amount,
            Millis(at),
        )
        .expect("entry")
    };
    let mut sink = VecQuarantine::default();

    // Inside the grant's life: the first entry fits, the second would take the node past its
    // quota and is quarantined.
    let inside = spot_shared_balance::Batch {
        node: node("n"),
        entries: vec![mk(1, 80, 10), mk(2, 30, 20)],
        complete: false,
    };
    let outcome = policy
        .settle(&mut hub, &inside, Millis(20), &mut sink)
        .expect("settle");
    assert_eq!(outcome.applied.len(), 1);

    // Head office receives an entry after the grant expired: expiry is judged on head office's
    // receive time, so a node's own time — here a backdating-friendly one — buys nothing.
    let late = spot_shared_balance::Batch {
        node: node("n"),
        entries: vec![mk(3, 5, 600)],
        complete: false,
    };
    let outcome = policy
        .settle(&mut hub, &late, Millis(700), &mut sink)
        .expect("settle");
    assert!(outcome.applied.is_empty());

    let reasons: Vec<RejectReason> = sink.held.iter().map(|(_, r)| *r).collect();
    assert_eq!(
        reasons,
        vec![RejectReason::OverQuota, RejectReason::AfterExpiry]
    );
    assert_eq!(hub.balance(&card()), Some(920));
}

#[test]
fn accept_and_reconcile_accepts_offline_within_the_cap() {
    let policy = AcceptAndReconcile::new(100, 10).expect("policy");
    let mut view = NodeView::new(node("b"));
    assert_eq!(
        policy.authorise(&mut view, &card(), 10, Millis(1)),
        Decision::Declined(DeclineReason::UnknownBalance)
    );
    view.learn_balance(&card(), 1000, Millis(0));
    assert!(
        policy
            .authorise(&mut view, &card(), 60, Millis(2))
            .is_approved()
    );
    assert!(
        policy
            .authorise(&mut view, &card(), 40, Millis(3))
            .is_approved()
    );
    assert_eq!(
        policy.authorise(&mut view, &card(), 1, Millis(4)),
        Decision::Declined(DeclineReason::OverRiskCap)
    );
    assert_eq!(policy.limits().max_overspend, 1000);
    assert!(policy.limits().offline_spend);
}

#[test]
fn accept_and_reconcile_declines_when_the_last_view_is_empty() {
    let policy = AcceptAndReconcile::new(100, 10).expect("policy");
    let mut view = NodeView::new(node("b"));
    view.learn_balance(&card(), 0, Millis(0));
    assert_eq!(
        policy.authorise(&mut view, &card(), 1, Millis(1)),
        Decision::Declined(DeclineReason::NoFunds)
    );
}

#[test]
fn shortfalls_become_receivables_and_the_balance_returns_to_zero() {
    let mut hub = hub_with(100);
    let policy = AcceptAndReconcile::new(100, 2).expect("policy");
    let mut a = NodeView::new(node("a"));
    let mut b = NodeView::new(node("b"));
    a.learn_balance(&card(), 100, Millis(0));
    b.learn_balance(&card(), 100, Millis(0));
    assert!(
        policy
            .authorise(&mut a, &card(), 90, Millis(1))
            .is_approved()
    );
    assert!(
        policy
            .authorise(&mut b, &card(), 90, Millis(2))
            .is_approved()
    );

    let mut sink = VecQuarantine::default();
    let first = policy
        .settle(&mut hub, &a.progress_batch(), Millis(10), &mut sink)
        .expect("settle a");
    assert!(first.receivables.is_empty());
    let second = policy
        .settle(&mut hub, &b.progress_batch(), Millis(11), &mut sink)
        .expect("settle b");
    assert_eq!(second.receivables.len(), 1);
    assert_eq!(second.receivables[0].amount, 80);
    assert_eq!(second.receivables[0].node, node("b"));
    assert_eq!(hub.balance(&card()), Some(0));
    assert_eq!(hub.receivable_total(), 80);
    assert!(hub.receivable_total() <= policy.limits().max_overspend);
    assert!(sink.held.is_empty());
}

#[test]
fn accept_and_reconcile_bound_is_reached_but_never_exceeded() {
    // 50 branches all hold a stale view of a nearly empty card and each sells up to its cap.
    let cap = 50;
    let nodes = 50_u32;
    let mut hub = hub_with(100);
    let policy = AcceptAndReconcile::new(cap, nodes).expect("policy");
    let mut views: Vec<NodeView> = (0..nodes)
        .map(|i| {
            let mut v = NodeView::new(node(&format!("n{i}")));
            v.learn_balance(&card(), 100, Millis(0));
            v
        })
        .collect();
    for v in &mut views {
        assert!(policy.authorise(v, &card(), cap, Millis(1)).is_approved());
        assert!(!policy.authorise(v, &card(), 1, Millis(2)).is_approved());
    }
    let mut sink = VecQuarantine::default();
    for v in &mut views {
        let outcome = policy
            .settle(&mut hub, &v.progress_batch(), Millis(10), &mut sink)
            .expect("settle");
        v.apply_hub_reply(&outcome);
        v.learn_balance(&card(), hub.balance(&card()).unwrap_or(0), Millis(10));
    }
    assert_eq!(hub.receivable_total(), cap * i64::from(nodes) - 100);
    assert!(hub.receivable_total() <= policy.limits().max_overspend);
    // Everyone syncs again, sees a balance of zero and stops.
    let final_balance = hub.balance(&card()).unwrap_or(0);
    for v in &mut views {
        v.learn_balance(&card(), final_balance, Millis(15));
        assert_eq!(
            policy.authorise(v, &card(), 1, Millis(20)),
            Decision::Declined(DeclineReason::NoFunds)
        );
    }
}

#[test]
fn a_node_can_only_send_debits() {
    let mut hub = hub_with(100);
    let policy = AcceptAndReconcile::new(100, 1).expect("policy");
    let credit = Entry::new(
        EntryId {
            node: node("x"),
            seq: 1,
        },
        card(),
        EntryKind::Credit,
        999,
        Millis(1),
    )
    .expect("entry");
    let batch = spot_shared_balance::Batch {
        node: node("x"),
        entries: vec![credit],
        complete: false,
    };
    let mut sink = VecQuarantine::default();
    policy
        .settle(&mut hub, &batch, Millis(2), &mut sink)
        .expect("settle");
    assert_eq!(sink.held[0].1, RejectReason::NotPermitted);
    assert_eq!(hub.balance(&card()), Some(100));
}

#[test]
fn an_entry_for_an_unknown_balance_is_quarantined_not_dropped() {
    let mut hub = hub_with(100);
    let policy = AllocatedQuota;
    let entry = Entry::new(
        EntryId {
            node: node("x"),
            seq: 1,
        },
        BalanceId::new("no-such-card"),
        EntryKind::Debit,
        5,
        Millis(1),
    )
    .expect("entry");
    let batch = spot_shared_balance::Batch {
        node: node("x"),
        entries: vec![entry],
        complete: false,
    };
    let mut sink = VecQuarantine::default();
    policy
        .settle(&mut hub, &batch, Millis(2), &mut sink)
        .expect("settle");
    assert_eq!(sink.held[0].1, RejectReason::UnknownBalance);
}

#[test]
fn the_ledger_is_idempotent_and_rejects_a_reused_id() {
    let mut ledger = Ledger::new();
    let id = EntryId {
        node: node("n"),
        seq: 1,
    };
    let entry = Entry::new(id.clone(), card(), EntryKind::Credit, 50, Millis(1)).expect("entry");
    assert_eq!(
        ledger.append(entry.clone()),
        Ok(spot_shared_balance::Appended::New)
    );
    assert_eq!(
        ledger.append(entry),
        Ok(spot_shared_balance::Appended::Duplicate)
    );
    assert_eq!(ledger.balance(), 50);
    let changed = Entry::new(id, card(), EntryKind::Credit, 51, Millis(1)).expect("entry");
    assert!(matches!(
        ledger.append(changed),
        Err(BalanceError::EntryConflict { .. })
    ));
    assert_eq!(ledger.balance(), 50);
    assert_eq!(ledger.recomputed_balance(), Some(50));
}

#[test]
fn the_ledger_refuses_to_overflow() {
    let mut ledger = Ledger::new();
    let mk = |seq: u64, amount: i64| {
        Entry::new(
            EntryId {
                node: node("n"),
                seq,
            },
            card(),
            EntryKind::Credit,
            amount,
            Millis(1),
        )
        .expect("entry")
    };
    ledger.append(mk(1, i64::MAX)).expect("first");
    assert_eq!(ledger.append(mk(2, 1)), Err(BalanceError::Overflow));
    assert_eq!(ledger.entries().len(), 1);
}

#[test]
fn the_default_policy_is_online_only_and_provisional() {
    assert_eq!(PROVISIONAL_DEFAULT, PolicyKind::OnlineOnly);
    let mut table = PolicyTable::new();
    assert_eq!(table.kind_for("gift_card"), PolicyKind::OnlineOnly);
    table
        .set("loyalty_points", PolicyKind::AllocatedQuota)
        .expect("set");
    assert_eq!(table.kind_for("loyalty_points"), PolicyKind::AllocatedQuota);
    assert_eq!(table.kind_for("gift_card"), PolicyKind::OnlineOnly);
    assert!(matches!(
        table.set(
            "x",
            PolicyKind::AcceptAndReconcile {
                risk_cap: 0,
                max_nodes: 1
            }
        ),
        Err(BalanceError::Config { .. })
    ));
    assert!(AcceptAndReconcile::new(10, 0).is_err());
}

#[test]
fn every_code_is_a_dotted_shared_balance_code() {
    let reasons = [
        DeclineReason::Offline,
        DeclineReason::Insufficient,
        DeclineReason::NoQuota,
        DeclineReason::QuotaExpired,
        DeclineReason::QuotaExhausted,
        DeclineReason::OverRiskCap,
        DeclineReason::NoFunds,
        DeclineReason::InvalidAmount,
        DeclineReason::UnknownBalance,
    ];
    for r in reasons {
        assert!(r.code().starts_with("shared_balance.declined_"));
    }
    assert!(BalanceError::Overflow.code().starts_with("shared_balance."));
    assert!(
        RejectReason::NoGrant
            .code()
            .starts_with("shared_balance.rejected_")
    );
}

#[test]
fn a_batch_cannot_carry_another_nodes_entries() {
    let policy = AllocatedQuota;
    let mut hub = hub_with(1000);
    hub.grant_quota(&node("victim"), &card(), 500, Millis(1000), Millis(0))
        .expect("grant");
    let stolen = Entry::new(
        EntryId {
            node: node("victim"),
            seq: 1,
        },
        card(),
        EntryKind::Debit,
        400,
        Millis(10),
    )
    .expect("entry");
    let batch = spot_shared_balance::Batch {
        node: node("attacker"),
        entries: vec![stolen.clone()],
        complete: false,
    };
    let mut sink = VecQuarantine::default();
    let outcome = policy
        .settle(&mut hub, &batch, Millis(20), &mut sink)
        .expect("settle");
    assert!(outcome.applied.is_empty());
    assert!(outcome.handled().is_empty());
    assert_eq!(outcome.rejected_foreign, vec![stolen.id.clone()]);
    assert_eq!(sink.held.len(), 1);
    assert_eq!(hub.balance(&card()), Some(1000));
    // The real owner can still report an entry under that id.
    let own = spot_shared_balance::Batch {
        node: node("victim"),
        entries: vec![stolen],
        complete: false,
    };
    let outcome = policy
        .settle(&mut hub, &own, Millis(30), &mut sink)
        .expect("settle");
    assert_eq!(outcome.applied.len(), 1);
    assert_eq!(hub.balance(&card()), Some(600));
}

#[test]
fn a_changed_copy_of_a_held_entry_is_quarantined_not_taken_as_a_repeat() {
    let policy = AcceptAndReconcile::new(100, 10).expect("policy");
    let mut hub = hub_with(1000);
    let mk = |amount| {
        Entry::new(
            EntryId {
                node: node("n"),
                seq: 1,
            },
            card(),
            EntryKind::Debit,
            amount,
            Millis(5),
        )
        .expect("entry")
    };
    let mut sink = VecQuarantine::default();
    for amount in [10, 90] {
        let batch = spot_shared_balance::Batch {
            node: node("n"),
            entries: vec![mk(amount)],
            complete: false,
        };
        policy
            .settle(&mut hub, &batch, Millis(6), &mut sink)
            .expect("settle");
    }
    assert_eq!(hub.balance(&card()), Some(990));
    assert_eq!(sink.held.len(), 1);
    assert_eq!(sink.held[0].1, RejectReason::NotPermitted);
}
