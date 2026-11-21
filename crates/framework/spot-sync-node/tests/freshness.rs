#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Horizons confirm freshness classes the applied prefix covers.
mod common;

use std::collections::BTreeMap;

use common::*;

#[test]
fn horizon_confirms_only_covered_classes() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("a", 10, &[])])), 0)
        .expect("ok");
    let mut horizons = BTreeMap::new();
    horizons.insert("posting".to_owned(), 10_u64);
    horizons.insert("catalog".to_owned(), 11_u64);
    let confirmed = n.on_horizons(TENANT, &horizons, 1_000);
    assert_eq!(confirmed, vec!["posting".to_owned()]);
    assert_eq!(
        n.freshness(TENANT)
            .expect("tenant")
            .elapsed("posting", 1_500),
        Some(500)
    );
    assert_eq!(
        n.freshness(TENANT)
            .expect("tenant")
            .elapsed("catalog", 1_500),
        None
    );
    assert_eq!(
        n.freshness(TENANT)
            .expect("tenant")
            .confirmed_horizons()
            .get("posting"),
        Some(&10)
    );
}

#[test]
fn nothing_applied_confirms_nothing() {
    let mut n = intake();
    let mut horizons = BTreeMap::new();
    horizons.insert("posting".to_owned(), 0_u64);
    assert!(n.on_horizons(TENANT, &horizons, 5).is_empty());
}

#[test]
fn a_parked_bundle_keeps_the_class_unconfirmed() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("early", 5, &["gone"])])), 0)
        .expect("ok");
    n.receive(deliver(bundle(vec![rec("late", 9, &[])])), 1)
        .expect("ok");
    let mut horizons = BTreeMap::new();
    horizons.insert("posting".to_owned(), 9_u64);
    assert!(n.on_horizons(TENANT, &horizons, 5).is_empty());
}
