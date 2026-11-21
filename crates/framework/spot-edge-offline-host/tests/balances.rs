//! The edge's balance gate: the default is online only; an offline spend follows the policy.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;

use spot_edge_offline_host::balance::{BALANCE_ONLINE_ONLY, BalancePolicy};
use spot_edge_offline_host::edge_balances::EdgeBalances;
use spot_shared_balance::{BalanceId, Millis, NodeId};

fn balances(default: BalancePolicy, overrides: &[(&str, BalancePolicy)]) -> EdgeBalances {
    EdgeBalances::new(
        NodeId::new("edge-1"),
        default,
        overrides
            .iter()
            .map(|(id, policy)| ((*id).to_owned(), *policy))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[test]
fn online_only_is_the_default_and_refuses_an_offline_spend() {
    let b = balances(BalancePolicy::default(), &[]);
    let gift = BalanceId::new("gift-card");
    assert!(b.check(&gift, false).is_ok(), "online the hub decides");
    let error = b.check(&gift, true).unwrap_err();
    assert!(error.to_string().starts_with(BALANCE_ONLINE_ONLY));
}

#[test]
fn per_balance_overrides_win_over_the_default() {
    let b = balances(
        BalancePolicy::OnlineOnly,
        &[("credit", BalancePolicy::AcceptAndReconcile)],
    );
    assert!(b.check(&BalanceId::new("credit"), true).is_ok());
    assert!(b.check(&BalanceId::new("gift-card"), true).is_err());
}

#[test]
fn learning_a_balance_is_recorded_and_an_unallocated_quota_refuses() {
    let mut b = balances(BalancePolicy::AllocatedQuota, &[]);
    let credit = BalanceId::new("credit");
    b.learn(&credit, 500, Millis(10));
    assert_eq!(b.view().known(&credit), Some(500));
    // No quota was granted: nothing may be spent offline.
    assert!(b.check(&credit, true).is_err());
    assert_eq!(b.pending(), 0);
    assert!(b.delivery().entries.is_empty());
}
