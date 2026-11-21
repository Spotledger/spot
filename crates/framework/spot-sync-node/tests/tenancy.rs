#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Two tenants on one node, with the same entity and id: nothing one tenant applied satisfies
//! the other's reference, and neither tenant's settled positions move the other's prefix.
mod common;

use common::*;
use proptest::prelude::*;
use spot_sync_closure::Bundle;
use spot_sync_node::{
    BundleApply, MemoryParking, NodeError, NodeIntake, ParkingLimits, ParkingStore, Receipt,
};

const A: &str = "tenant-a";
const B: &str = "tenant-b";

#[test]
fn a_record_of_a_does_not_satisfy_a_reference_of_b() {
    let mut n = intake();
    n.receive(deliver_to(A, bundle(vec![rec("p", 1, &[])])), 0)
        .expect("ok");
    let r = n
        .receive(deliver_to(B, bundle(vec![rec("c", 1, &["p"])])), 1)
        .expect("ok");
    assert_eq!(
        r,
        Receipt::Parked {
            missing: vec![key("p")]
        }
    );
    assert!(!n.applier().has(B, &key("c"), 0));
    assert!(n.applier().has(A, &key("p"), 1));
    assert!(!n.applier().has(B, &key("p"), 0));
    assert_eq!(n.parking().len(B), 1);
    assert_eq!(n.applied_prefix(B), None);

    // B's own record satisfies it.
    n.receive(deliver_to(B, bundle(vec![rec("p", 0, &[])])), 2)
        .expect("ok");
    assert!(n.applier().has(B, &key("c"), 1));
}

#[test]
fn an_apply_wakes_only_the_same_tenants_parked_bundles() {
    let mut n = intake();
    n.receive(deliver_to(A, bundle(vec![rec("ca", 5, &["p"])])), 0)
        .expect("ok");
    n.receive(deliver_to(B, bundle(vec![rec("cb", 5, &["p"])])), 1)
        .expect("ok");
    let r = n
        .receive(deliver_to(A, bundle(vec![rec("p", 2, &[])])), 2)
        .expect("ok");
    match r {
        Receipt::Applied(a) => assert_eq!(a.released.len(), 1),
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(n.applier().has(A, &key("ca"), 5));
    assert!(!n.applier().has(B, &key("cb"), 0));
    assert_eq!(n.parking().len(A), 0);
    assert_eq!(n.parking().len(B), 1);
    assert_eq!(n.applied_prefix(A), Some(5));
    assert_eq!(n.applied_prefix(B), None);
}

#[test]
fn positions_of_one_tenant_do_not_advance_the_other_prefix() {
    let mut n = intake();
    n.receive(deliver_to(A, bundle(vec![rec("a1", 1, &[])])), 0)
        .expect("ok");
    n.receive(deliver_to(A, bundle(vec![rec("a5", 5, &["gone"])])), 1)
        .expect("ok");
    for (i, pos) in (5..=10).enumerate() {
        n.receive(
            deliver_to(B, bundle(vec![rec(&format!("b{pos}"), pos, &[])])),
            2 + i as u64,
        )
        .expect("ok");
    }
    assert_eq!(n.applied_prefix(A), Some(1));
    assert_eq!(n.applied_prefix(B), Some(10));
    assert_eq!(n.applied_prefix("unknown"), None);
}

#[test]
fn a_refused_bundle_holds_only_its_own_tenants_prefix() {
    let mut n = intake();
    n.applier_mut().refuse_tenants.insert(A.to_owned());
    n.receive(deliver_to(A, bundle(vec![rec("a3", 3, &[])])), 0)
        .expect("ok");
    n.receive(deliver_to(B, bundle(vec![rec("b3", 3, &[])])), 1)
        .expect("ok");
    n.receive(deliver_to(B, bundle(vec![rec("b4", 4, &[])])), 2)
        .expect("ok");
    assert_eq!(n.applied_prefix(A), None);
    assert_eq!(n.applied_prefix(B), Some(4));

    // A's redelivery applies once the sink accepts it; B's hold set is untouched throughout.
    n.applier_mut().refuse_tenants.clear();
    n.receive(deliver_to(A, bundle(vec![rec("a3", 3, &[])])), 3)
        .expect("ok");
    assert_eq!(n.applied_prefix(A), Some(3));
    assert_eq!(n.applied_prefix(B), Some(4));
}

#[test]
fn a_repaired_bundle_is_released_only_by_its_own_tenant() {
    let mut n = intake();
    n.receive(deliver_to(A, bundle(vec![rec("ca", 5, &["p"])])), 0)
        .expect("ok");
    n.receive(deliver_to(B, bundle(vec![rec("cb", 5, &["p"])])), 1)
        .expect("ok");
    n.expire(u64::MAX);
    assert_eq!(n.repair_len(), 2);
    // B gets its own `p` and `cb`, which clears B's repair item and no other.
    n.receive(deliver_to(B, bundle(vec![rec("p", 1, &[])])), 2)
        .expect("ok");
    n.receive(deliver_to(B, bundle(vec![rec("cb", 5, &["p"])])), 3)
        .expect("ok");
    let items = n.take_repair();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tenant, A);
    assert_eq!(n.applied_prefix(A), None);
    assert_eq!(n.applied_prefix(B), Some(5));
}

#[test]
fn freshness_and_skew_holds_are_per_tenant() {
    use std::collections::BTreeMap;
    let mut n = intake();
    n.receive(deliver_to(A, bundle(vec![rec("a", 10, &[])])), 0)
        .expect("ok");
    n.receive(deliver_to(B, bundle(vec![rec("b", 3, &[])])), 0)
        .expect("ok");
    let mut horizons = BTreeMap::new();
    horizons.insert("posting".to_owned(), 10_u64);
    assert_eq!(n.on_horizons(B, &horizons, 5), Vec::<String>::new());
    assert_eq!(n.on_horizons(A, &horizons, 5), vec!["posting".to_owned()]);
    assert!(n.freshness(A).expect("a").elapsed("posting", 6).is_some());
    assert_eq!(n.freshness(B).expect("b").elapsed("posting", 6), None);
    assert!(n.freshness("unknown").is_none());
    assert!(n.on_horizons("unknown", &horizons, 5).is_empty());
    assert_eq!(n.tenant_count(), 2);
}

#[test]
fn a_new_tenant_beyond_the_cap_is_refused_and_stores_nothing() {
    let mut config = config(ParkingLimits::default());
    config.max_tenants = 2;
    let mut n = NodeIntake::new(
        config,
        MemApply::default(),
        MemoryParking::new(),
        JsonCatalog,
        report(),
    )
    .expect("config");
    n.receive(deliver_to(A, bundle(vec![rec("a", 1, &[])])), 0)
        .expect("ok");
    n.receive(deliver_to(B, bundle(vec![rec("b", 1, &[])])), 0)
        .expect("ok");
    let err = n
        .receive(deliver_to("tenant-c", bundle(vec![rec("c", 1, &[])])), 0)
        .expect_err("refused");
    assert_eq!(err, NodeError::TooManyTenants { limit: 2 });
    assert_eq!(err.code(), "sync_node.too_many_tenants");
    assert_eq!(n.tenant_count(), 2);
    assert!(!n.applier().has("tenant-c", &key("c"), 0));
    assert_eq!(n.applied_prefix("tenant-c"), None);
    // Known tenants keep working.
    n.receive(deliver_to(A, bundle(vec![rec("a2", 2, &[])])), 1)
        .expect("ok");
    assert_eq!(n.applied_prefix(A), Some(2));
}

#[test]
fn a_zero_tenant_cap_is_rejected() {
    let mut config = config(ParkingLimits::default());
    config.max_tenants = 0;
    let built = NodeIntake::new(
        config,
        MemApply::default(),
        MemoryParking::new(),
        JsonCatalog,
        report(),
    );
    assert!(matches!(built, Err(NodeError::InvalidLimits { .. })));
}

/// Bundle i holds record `r{i}` and references the records of earlier bundles in `deps[i]`.
fn build(deps: &[Vec<usize>]) -> Vec<Bundle> {
    deps.iter()
        .enumerate()
        .map(|(i, d)| {
            let ids: Vec<String> = d
                .iter()
                .filter(|j| **j < i)
                .map(|j| format!("r{j}"))
                .collect();
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            bundle(vec![rec(&format!("r{i}"), (i as u64 + 1) * 10, &refs)])
        })
        .collect()
}

const N: usize = 5;

fn deps_strategy() -> impl Strategy<Value = Vec<Vec<usize>>> {
    prop::collection::vec(prop::collection::vec(0..N, 0..3), N)
}

proptest! {
    /// Interleaving two tenants' deliveries gives each tenant exactly the outcome it would have
    /// had alone on a fresh node (receipts, applied set and acknowledged prefix).
    #[test]
    fn each_tenant_outcome_equals_running_alone(
        deps_a in deps_strategy(),
        deps_b in deps_strategy(),
        picks in prop::collection::vec((any::<bool>(), 0..N), 1..30),
    ) {
        let bundles_a = build(&deps_a);
        let bundles_b = build(&deps_b);
        let seq: Vec<(&str, &Bundle)> = picks
            .iter()
            .map(|(second, i)| if *second { (B, &bundles_b[*i]) } else { (A, &bundles_a[*i]) })
            .collect();

        let mut joint = intake();
        let mut joint_receipts: std::collections::BTreeMap<&str, Vec<Receipt>> = Default::default();
        for (t, (tenant, b)) in seq.iter().enumerate() {
            let r = joint.receive(deliver_to(tenant, (*b).clone()), t as u64).expect("ok");
            joint_receipts.entry(*tenant).or_default().push(r);
        }

        for tenant in [A, B] {
            let mut alone = intake();
            let mut receipts = Vec::new();
            for (t, (_, b)) in seq.iter().enumerate().filter(|(_, (x, _))| *x == tenant) {
                receipts.push(alone.receive(deliver_to(tenant, (*b).clone()), t as u64).expect("ok"));
            }
            prop_assert_eq!(joint_receipts.get(tenant).cloned().unwrap_or_default(), receipts);
            prop_assert_eq!(joint.applied_prefix(tenant), alone.applied_prefix(tenant));
            let mine = |m: &std::collections::BTreeMap<(String, spot_sync_closure::RecordKey), u64>| {
                m.iter().filter(|((t, _), _)| t == tenant).map(|(k, v)| (k.1.clone(), *v)).collect::<Vec<_>>()
            };
            prop_assert_eq!(mine(&joint.applier().stored), mine(&alone.applier().stored));
            prop_assert_eq!(joint.parking().len(tenant), alone.parking().len(tenant));
        }
    }
}
