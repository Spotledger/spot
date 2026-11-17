#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! WP-MET-03.T7: the rule every path uses to mint the version of a change. It is never 1, because
//! the starting snapshot `Version::ZERO` already reports metadata version 1, so the first change
//! of a fresh site must change the served number too.

use spot_core::SiteId;
use spot_meta::{Definition, FieldSpec, FieldType, Property};
use spot_meta_live::{LiveMeta, MetaChange, Version, next_change_version};

fn site() -> SiteId {
    SiteId::new("acme.example").unwrap()
}

fn widget() -> Definition {
    let mut definition = Definition::new("widget");
    definition
        .fields
        .push(FieldSpec::new("title", FieldType::Text));
    definition
}

#[test]
fn the_next_change_version_is_never_one() {
    assert_eq!(next_change_version(Version::ZERO), Some(Version::new(2)));
    assert_eq!(next_change_version(Version::new(1)), Some(Version::new(2)));
    assert_eq!(next_change_version(Version::new(2)), Some(Version::new(3)));
    assert_eq!(next_change_version(Version::new(7)), Some(Version::new(8)));
}

#[test]
fn the_version_space_is_not_wrapped() {
    assert_eq!(next_change_version(Version::new(u64::MAX)), None);
    assert_eq!(
        next_change_version(Version::new(u64::MAX - 1)),
        Some(Version::new(u64::MAX))
    );
}

#[test]
fn a_version_always_rises() {
    let mut previous = Version::ZERO;
    for _ in 0..50 {
        let next = next_change_version(previous).unwrap();
        assert!(next > previous);
        previous = next;
    }
}

#[test]
fn the_first_apply_on_a_fresh_site_raises_the_served_metadata_version() {
    let live = LiveMeta::new(site(), vec![widget()]).unwrap();
    let starting = live.current();
    // The starting snapshot reports metadata version 1.
    assert_eq!(starting.meta().version(), 1);
    let minted = live
        .apply(MetaChange::SetProperty {
            doctype: "widget".into(),
            field: Some("title".into()),
            property: Property::Label("A".into()),
        })
        .unwrap();
    assert_eq!(minted, Version::new(2));
    let after = live.current();
    assert!(after.meta().version() > starting.meta().version());
    assert_eq!(after.meta().version(), after.version().get());
}
