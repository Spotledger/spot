//! Coverage decisions of `MasterScope`, and a property that it agrees with `Subscription::covers`.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use proptest::prelude::*;
use spot_sync_closure::RecordKey;
use spot_sync_hub::{DestinationScope, EntryRole, StreamEntry};
use spot_sync_hub_adapters::{MasterScope, ScopeKeysOf, encode_scope_key};
use spot_sync_master::{Doctype, MasterChange, ScopeKey, Subscription};
use spot_sync_obligations::Lane;
use spot_sync_protocol::{NodeId, RecordName};
use std::collections::BTreeMap;

#[derive(Default)]
struct Registry(BTreeMap<RecordKey, Vec<String>>);

impl ScopeKeysOf for Registry {
    fn scope_keys(&self, entry_key: &RecordKey) -> Option<Vec<String>> {
        self.0.get(entry_key).cloned()
    }
}

fn sk(t: &str, v: &str) -> ScopeKey {
    ScopeKey::new(t, v).unwrap()
}

fn sub(keys: &[(&str, &str)], doctypes: &[&str]) -> Subscription {
    Subscription::new(
        NodeId::new("edge-1").unwrap(),
        keys.iter().map(|(t, v)| sk(t, v)),
        doctypes.iter().map(|d| Doctype::new(d).unwrap()),
    )
}

fn entry(entity: &str, id: &str, version: u64) -> StreamEntry {
    StreamEntry {
        position: 1,
        tenant: "t1".into(),
        source: "hub".into(),
        origin: None,
        txn: "x".into(),
        role: EntryRole::Root,
        key: RecordKey::new(entity, id),
        version,
        topic: entity.into(),
        major: 1,
        lane: Lane::Bulk,
    }
}

fn registry(entity: &str, id: &str, keys: &[&str]) -> Registry {
    let mut r = Registry::default();
    r.0.insert(
        RecordKey::new(entity, id),
        keys.iter().map(|s| (*s).to_owned()).collect(),
    );
    r
}

#[test]
fn covered_when_subscribed_and_holding_every_key() {
    let s = sub(&[("company", "C1"), ("warehouse", "W7")], &["price_list"]);
    let r = registry("price_list", "PL-1", &["company:C1", "warehouse:W7"]);
    let scope = MasterScope::new(&s, &r);
    assert!(scope.covers(&entry("price_list", "PL-1", 3)));
    assert!(scope.covers_key(&RecordKey::new("price_list", "PL-1")));
}

#[test]
fn not_covered_when_one_key_is_missing_or_entity_not_subscribed() {
    let s = sub(&[("company", "C1")], &["price_list"]);
    let r = registry("price_list", "PL-1", &["company:C1", "warehouse:W7"]);
    let scope = MasterScope::new(&s, &r);
    assert!(!scope.covers(&entry("price_list", "PL-1", 1)));
    assert!(!scope.covers_key(&RecordKey::new("price_list", "PL-1")));

    let s2 = sub(&[("company", "C1")], &["other"]);
    let r2 = registry("price_list", "PL-1", &["company:C1"]);
    assert!(!MasterScope::new(&s2, &r2).covers(&entry("price_list", "PL-1", 1)));
}

#[test]
fn empty_requirement_is_covered_by_any_subscriber_of_the_entity() {
    let s = sub(&[], &["price_list"]);
    let r = registry("price_list", "PL-1", &[]);
    assert!(MasterScope::new(&s, &r).covers(&entry("price_list", "PL-1", 1)));
}

#[test]
fn unknown_scope_keys_are_not_covered() {
    let s = sub(&[("company", "C1")], &["price_list"]);
    let r = Registry::default();
    let scope = MasterScope::new(&s, &r);
    assert!(!scope.covers(&entry("price_list", "PL-1", 1)));
    assert!(!scope.covers_key(&RecordKey::new("price_list", "PL-1")));
}

#[test]
fn invalid_names_versions_and_key_text_are_not_covered_and_do_not_panic() {
    let s = sub(&[("company", "C1")], &["price_list"]);
    for (entity, id) in [
        ("Price List", "PL-1"),
        ("", "PL-1"),
        ("price_list", ""),
        ("price_list", "a\nb"),
    ] {
        let r = registry(entity, id, &["company:C1"]);
        let scope = MasterScope::new(&s, &r);
        assert!(!scope.covers(&entry(entity, id, 1)), "{entity}/{id}");
        assert!(!scope.covers_key(&RecordKey::new(entity, id)));
    }
    // Version 0 is not a valid version.
    let r = registry("price_list", "PL-1", &[]);
    assert!(!MasterScope::new(&s, &r).covers(&entry("price_list", "PL-1", 0)));
    // Key text without a colon, or with an empty part, is not a scope key.
    for bad in ["company", ":C1", "company:"] {
        let r = registry("price_list", "PL-1", &[bad]);
        assert!(!MasterScope::new(&s, &r).covers(&entry("price_list", "PL-1", 1)));
    }
}

#[test]
fn master_data_is_never_owned_by_a_destination() {
    let s = sub(&[], &["price_list"]);
    let r = Registry::default();
    assert!(!MasterScope::new(&s, &r).owns(&RecordKey::new("price_list", "PL-1")));
}

#[test]
fn a_key_type_with_a_colon_has_no_text_form() {
    assert_eq!(encode_scope_key(&sk("a:b", "c")), None);
    assert_eq!(encode_scope_key(&sk("a", "b:c")).as_deref(), Some("a:b:c"));
}

fn key_strategy() -> impl Strategy<Value = ScopeKey> {
    ("[a-c]", "[x-z]").prop_map(|(t, v)| ScopeKey::new(&t, &v).unwrap())
}

proptest! {
    #[test]
    fn agrees_with_subscription_covers(
        held in proptest::collection::vec(key_strategy(), 0..5),
        required in proptest::collection::vec(key_strategy(), 0..5),
        subscribed in any::<bool>(),
        version in 1u64..1000,
    ) {
        let doctypes: &[&str] = if subscribed { &["price_list"] } else { &["other"] };
        let s = Subscription::new(
            NodeId::new("edge-1").unwrap(),
            held,
            doctypes.iter().map(|d| Doctype::new(d).unwrap()),
        );
        let texts: Vec<String> = required.iter().map(|k| encode_scope_key(k).unwrap()).collect();
        let mut r = Registry::default();
        r.0.insert(RecordKey::new("price_list", "PL-1"), texts);
        let change = MasterChange::new(
            Doctype::new("price_list").unwrap(),
            RecordName::new("PL-1").unwrap(),
            version,
            required,
            "",
        ).unwrap();
        let scope = MasterScope::new(&s, &r);
        prop_assert_eq!(scope.covers(&entry("price_list", "PL-1", version)), s.covers(&change));
        prop_assert_eq!(scope.covers_key(&RecordKey::new("price_list", "PL-1")), s.covers(&change));
    }
}
