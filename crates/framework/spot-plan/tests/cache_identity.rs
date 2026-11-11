//! The plan cache keyed on the metadata **identity**, not its version: two independently
//! built `EffectiveMeta` values can both report version 1 while describing different
//! metadata, and a refresh must never keep serving plans built from the other one.
#![allow(missing_docs)]

mod common;

use std::sync::Arc;

use spot_meta::{FieldSpec, FieldType, Registry};
use spot_plan::{PlanCache, PlanSet};

/// The fixture definition set with one extra field appended to `invoice`.
fn definitions_with_extra_invoice_field(name: &str) -> Vec<spot_meta::Definition> {
    let mut defs = common::definitions();
    let invoice = defs
        .iter_mut()
        .find(|d| d.name == "invoice")
        .unwrap_or_else(|| panic!("the fixtures carry no invoice"));
    invoice.fields.push(FieldSpec::new(name, FieldType::Text));
    defs
}

#[test]
fn refresh_rebuilds_when_an_independently_built_metadata_carries_the_same_version() {
    // Two registries built over different definitions: both start at version 1.
    let first = common::registry().effective();
    let second = Registry::new(definitions_with_extra_invoice_field("custom_score"))
        .unwrap_or_else(|e| panic!("the fixtures do not build: {e:?}"))
        .effective();
    assert_eq!(first.version(), 1);
    assert_eq!(
        second.version(),
        1,
        "the second metadata describes different definitions at the same version"
    );

    let cache = PlanCache::new(
        PlanSet::build(&first).unwrap_or_else(|e| panic!("plans do not compile: {e:?}")),
    );
    let invoice_before = cache
        .current()
        .get("invoice")
        .unwrap_or_else(|| panic!("no invoice"));
    assert!(
        invoice_before.field("custom_score").is_none(),
        "the first metadata has no custom field"
    );

    // The version did not move, but the metadata did: the invoice's plan must be rebuilt.
    let changed = cache
        .refresh(&second)
        .unwrap_or_else(|e| panic!("the refresh failed: {e:?}"));
    assert_eq!(
        changed,
        ["invoice"],
        "a same-version metadata change is not a no-op"
    );
    let after = cache.current();
    assert_eq!(
        after.meta_version(),
        1,
        "the version stays 1: it is information, not the cache key"
    );
    let invoice_after = after.get("invoice").unwrap_or_else(|| panic!("no invoice"));
    assert!(
        invoice_after.field("custom_score").is_some(),
        "the current plan carries the field of the metadata just refreshed"
    );
    assert!(
        !Arc::ptr_eq(&invoice_after, &invoice_before),
        "the stale plan is not served any more"
    );
    // The DocTypes the second metadata did not change keep their plans.
    assert_eq!(
        after
            .get("party")
            .unwrap_or_else(|| panic!("no party"))
            .fields()
            .len(),
        11
    );
}

#[test]
fn refreshing_the_same_metadata_twice_changes_nothing_and_reuses_every_plan() {
    let meta = common::registry().effective();
    let cache = PlanCache::new(
        PlanSet::build(&meta).unwrap_or_else(|e| panic!("plans do not compile: {e:?}")),
    );
    let names: Vec<String> = cache.current().doctypes().map(str::to_owned).collect();
    let before: Vec<_> = names
        .iter()
        .map(|name| {
            cache
                .current()
                .get(name)
                .unwrap_or_else(|| panic!("no plan for {name}"))
        })
        .collect();

    for round in 0..2 {
        let changed = cache
            .refresh(&meta)
            .unwrap_or_else(|e| panic!("the refresh failed: {e:?}"));
        assert!(
            changed.is_empty(),
            "round {round}: identical metadata changes no plan"
        );
        let set = cache.current();
        assert_eq!(set.meta_version(), meta.version());
        assert_eq!(set.doctypes().count(), names.len());
        for (name, old) in names.iter().zip(&before) {
            let new = set
                .get(name)
                .unwrap_or_else(|| panic!("no plan for {name} after a refresh"));
            assert!(
                Arc::ptr_eq(&new, old),
                "round {round}: {name}'s Arc is reused, so a held plan stays valid"
            );
        }
    }
}
