//! The `PolicyLookup` seam: what spot-partition and spot-sync-master consult.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{declared, doctype, master, text, transactional};
use spot_sync_policy::{Direction, PolicyLookup, PolicySet, ScopeKeyType, StaticPolicies, resolve};

#[test]
fn a_table_answers_the_seam() {
    let meta = common::build(vec![
        doctype("item", vec![text("title")]),
        doctype("till_sale", vec![text("total")]),
    ]);
    let mut set = PolicySet::new();
    set.declare("item", master());
    set.declare(
        "till_sale",
        transactional(Some(ScopeKeyType::new("warehouse").unwrap())),
    );
    let table = resolve(&meta, &set);

    assert!(PolicyLookup::is_synced(&table, "item"));
    assert_eq!(PolicyLookup::direction(&table, "item"), Direction::Down);
    assert_eq!(
        PolicyLookup::owner_scope(&table, "till_sale")
            .unwrap()
            .as_str(),
        "warehouse"
    );
    assert!(PolicyLookup::allows(&table, "item", Direction::Down));
    assert!(!PolicyLookup::allows(&table, "item", Direction::Up));
    assert!(PolicyLookup::allows(&table, "till_sale", Direction::Up));
}

#[test]
fn an_unknown_doctype_reads_as_local() {
    let table = resolve(&common::build(vec![]), &PolicySet::new());
    assert!(!PolicyLookup::is_synced(&table, "ghost"));
    assert_eq!(PolicyLookup::direction(&table, "ghost"), Direction::None);
    assert!(PolicyLookup::owner_scope(&table, "ghost").is_none());
    assert!(!PolicyLookup::allows(&table, "ghost", Direction::Up));
}

#[test]
fn both_allows_both_ways() {
    let mut policies = StaticPolicies::new();
    let mut shared = master();
    shared.direction = Direction::Both;
    policies.insert("shared_doc", shared);

    assert!(PolicyLookup::allows(&policies, "shared_doc", Direction::Up));
    assert!(PolicyLookup::allows(
        &policies,
        "shared_doc",
        Direction::Down
    ));
    assert!(!PolicyLookup::allows(
        &policies,
        "shared_doc",
        Direction::None
    ));
}

#[test]
fn a_static_set_answers_too() {
    let mut policies = StaticPolicies::new();
    policies.insert("item", master());
    assert!(PolicyLookup::is_synced(&policies, "item"));
    assert_eq!(
        policies.get("item").unwrap().class,
        spot_sync_policy::Class::Master
    );
}

// A one-DocType declaration helper is exercised here so the `declared` import is used.
#[test]
fn declared_helper_is_consistent() {
    let set = declared("item", master());
    assert_eq!(
        set.get("item").unwrap().class,
        spot_sync_policy::Class::Master
    );
}
