#![allow(clippy::expect_used)]
//! Recorded security finding: applying head office's reply learns the balance in the same call.
//!
//! The accept-and-reconcile bound holds only while a node keeps learning head office's total. When
//! applying a reply and learning the balance were two separate calls, a caller who made only the
//! first kept spending against an old, higher view. The reply now carries head office's totals and
//! is the only thing [`NodeView::apply_hub_reply`] takes, so the two cannot part.

use spot_shared_balance::{
    AcceptAndReconcile, BalanceId, BalancePolicy, Decision, DeclineReason, Hub, Millis, NodeId,
    NodeView, VecQuarantine,
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

#[test]
fn applying_the_reply_acknowledges_and_learns_in_one_call() {
    let mut hub = hub_with(1_000);
    let policy = AcceptAndReconcile::new(100, 2).expect("policy");
    let mut view = NodeView::new(node("branch-1"));
    // The last sync: a high view the node may spend against while offline.
    view.learn_balance(&card(), 1_000, Millis(0));
    let Decision::Approved(entry) = policy.authorise(&mut view, &card(), 60, Millis(1)) else {
        panic!("the spend is inside the cap");
    };

    let mut sink = VecQuarantine::default();
    let reply = policy
        .settle(&mut hub, &view.progress_batch(), Millis(10), &mut sink)
        .expect("settle");
    assert_eq!(reply.applied, vec![entry.id.clone()]);
    assert_eq!(
        reply.total(&card()).map(|total| total.total()),
        Some(940),
        "the reply carries head office's total"
    );

    view.apply_hub_reply(&reply);

    assert!(
        view.pending().is_empty(),
        "the delivery is acknowledged in the same call"
    );
    assert_eq!(
        view.known(&card()),
        Some(940),
        "and the node learned the total head office signed"
    );
    assert_eq!(
        view.known_at(&card()),
        Some(Millis(10)),
        "stamped with head office's time, not the node's"
    );
}

#[test]
fn every_applied_reply_leaves_the_node_with_head_offices_total() {
    // Ten rounds of spend, deliver and apply: after every round the node's view is head office's,
    // however much the balance moved. A caller cannot end a round with a stale view, because
    // applying the reply is what ends it.
    let mut hub = hub_with(1_000);
    let policy = AcceptAndReconcile::new(100, 3).expect("policy");
    let mut view = NodeView::new(node("branch-1"));
    view.learn_balance(&card(), 1_000, Millis(0));
    let mut sink = VecQuarantine::default();

    for round in 1..=10_u64 {
        while let Decision::Approved(_) = policy.authorise(&mut view, &card(), 10, Millis(round)) {}
        let reply = policy
            .settle(&mut hub, &view.progress_batch(), Millis(round), &mut sink)
            .expect("settle");
        view.apply_hub_reply(&reply);
        assert_eq!(
            view.known(&card()),
            hub.balance(&card()),
            "round {round}: the node holds head office's total"
        );
        assert!(
            view.pending().is_empty(),
            "round {round}: nothing outstanding"
        );
    }
    assert!(hub.receivable_total() <= policy.limits().max_overspend);
}

#[test]
fn a_node_that_applies_its_reply_stops_spending_a_balance_that_is_gone() {
    // Two nodes hold the same stale view and both spend their cap. Once the first has settled and
    // applied its reply, the second's reply shows an empty balance: applying it (which the type
    // does not let the caller skip) is what stops the second node from spending again.
    let mut hub = hub_with(100);
    let policy = AcceptAndReconcile::new(100, 2).expect("policy");
    let mut first = NodeView::new(node("branch-a"));
    let mut second = NodeView::new(node("branch-b"));
    first.learn_balance(&card(), 100, Millis(0));
    second.learn_balance(&card(), 100, Millis(0));
    assert!(
        policy
            .authorise(&mut first, &card(), 90, Millis(1))
            .is_approved()
    );
    assert!(
        policy
            .authorise(&mut second, &card(), 90, Millis(2))
            .is_approved()
    );

    let mut sink = VecQuarantine::default();
    let reply = policy
        .settle(&mut hub, &first.progress_batch(), Millis(3), &mut sink)
        .expect("settle a");
    first.apply_hub_reply(&reply);
    assert_eq!(first.known(&card()), Some(10));

    let reply = policy
        .settle(&mut hub, &second.progress_batch(), Millis(4), &mut sink)
        .expect("settle b");
    second.apply_hub_reply(&reply);
    assert_eq!(
        second.known(&card()),
        Some(0),
        "head office wrote the shortfall back"
    );
    assert_eq!(
        policy.authorise(&mut second, &card(), 1, Millis(5)),
        Decision::Declined(DeclineReason::NoFunds),
        "the learned total is what stops the next spend"
    );
    assert_eq!(hub.receivable_total(), 80);
}
