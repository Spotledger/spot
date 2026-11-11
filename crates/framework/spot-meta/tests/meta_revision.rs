#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! WP-MET-03.T3: `Customisations::stamp_revision` sets the revision exactly, never partially,
//! and the registry then reports `revision + 1`.

use spot_meta::{BuildOptions, Customisations, Definition, FieldSpec, FieldType, Registry};

fn widget() -> Definition {
    let mut d = Definition::new("widget");
    d.fields.push(FieldSpec::new("title", FieldType::Text));
    d
}

#[test]
fn stamp_revision_sets_the_revision_exactly() {
    let mut c = Customisations::new();
    c.add_field("widget", FieldSpec::new("custom_a", FieldType::Text), None);
    c.touch();
    assert_eq!(c.revision(), 2);

    c.stamp_revision(40);
    assert_eq!(c.revision(), 40);
    // It may also lower the number: the live layer owns the numbering.
    c.stamp_revision(1);
    assert_eq!(c.revision(), 1);
    // The content is untouched.
    assert_eq!(c.fields().len(), 1);
    c.stamp_revision(0);
    assert_eq!(c.revision(), Customisations::new().revision());
}

#[test]
fn the_registry_reports_the_stamped_revision_plus_one() {
    for revision in [0_u64, 1, 6, 1_000] {
        let mut c = Customisations::new();
        c.add_field("widget", FieldSpec::new("custom_a", FieldType::Text), None);
        c.stamp_revision(revision);
        let meta = Registry::build_with(vec![widget()], &c, &BuildOptions::new())
            .unwrap_or_else(|e| panic!("builds: {e:?}"));
        assert_eq!(meta.version(), revision + 1);
    }
}

#[test]
fn a_change_after_a_stamp_still_rises() {
    let mut c = Customisations::with_revision(3);
    c.stamp_revision(9);
    c.touch();
    assert_eq!(c.revision(), 10);
}
