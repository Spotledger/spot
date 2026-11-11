//! An effective snapshot rebuilds from its own definitions (what a follower node does), and the
//! metadata version can be reproduced from an agreed number.
#![allow(missing_docs)]

mod common;

use common::build;
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, ProtectionGroupSpec, Registry};

fn with_permlevel() -> Definition {
    let mut d = Definition::new("invoice");
    let mut a = FieldSpec::new("amount", FieldType::Decimal);
    a.scale = Some(2);
    a.permlevel = 2;
    let mut b = FieldSpec::new("cost", FieldType::Decimal);
    b.scale = Some(2);
    b.permlevel = 2;
    let mut c = FieldSpec::new("margin", FieldType::Decimal);
    c.scale = Some(2);
    c.permlevel = 1;
    d.fields = vec![a, b, c, FieldSpec::new("note", FieldType::Text)];
    d
}

fn views(m: &spot_meta::EffectiveMeta) -> String {
    let mut out = String::new();
    for v in m.doctypes() {
        out.push_str(&format!("{v:?}\n"));
    }
    out
}

#[test]
fn effective_definitions_with_permlevel_rebuild_to_the_same_content() {
    let first = build(vec![with_permlevel()]).unwrap_or_else(|e| panic!("{e:?}"));
    let again =
        build(first.definitions().to_vec()).unwrap_or_else(|e| panic!("rebuild failed: {e:?}"));
    assert_eq!(first.definitions(), again.definitions());
    assert_eq!(views(&first), views(&again));
    // The compatibility alias stays readable on the effective definition.
    let def = again
        .definition("invoice")
        .unwrap_or_else(|| panic!("missing"));
    assert_eq!(
        def.fields
            .iter()
            .find(|f| f.name == "amount")
            .map(|f| f.permlevel),
        Some(2)
    );
    // No duplicate level_N group, however many times it is rebuilt.
    let third = build(again.definitions().to_vec()).unwrap_or_else(|e| panic!("{e:?}"));
    let names: Vec<_> = third
        .definition("invoice")
        .unwrap_or_else(|| panic!("missing"))
        .protection_groups
        .iter()
        .map(|g| g.name.clone())
        .collect();
    assert_eq!(names, vec!["level_2".to_owned(), "level_1".to_owned()]);
    assert_eq!(first.field_protection(), third.field_protection());
}

#[test]
fn an_existing_level_group_is_reused_not_duplicated() {
    let mut d = with_permlevel();
    d.protection_groups = vec![ProtectionGroupSpec::new("level_2", "Finance only")];
    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let def = m.definition("invoice").unwrap_or_else(|| panic!("missing"));
    let n = def
        .protection_groups
        .iter()
        .filter(|g| g.name == "level_2")
        .count();
    assert_eq!(n, 1);
    let g = def
        .protection_groups
        .iter()
        .find(|g| g.name == "level_2")
        .unwrap_or_else(|| panic!("missing"));
    assert_eq!(g.label.as_deref(), Some("Finance only"));
}

#[test]
fn a_different_explicit_group_with_a_permlevel_is_still_refused() {
    for group in ["custom", "level_3", "level_1"] {
        let mut d = Definition::new("doc");
        let mut f = FieldSpec::new("field", FieldType::Text);
        f.permlevel = 2;
        f.protection_group = Some(group.into());
        d.fields = vec![f];
        d.protection_groups = vec![
            ProtectionGroupSpec::new("custom", "Custom"),
            ProtectionGroupSpec::new("level_3", "Three"),
            ProtectionGroupSpec::new("level_1", "One"),
        ];
        let errors = build(vec![d])
            .err()
            .unwrap_or_else(|| panic!("conflict must be refused"));
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("meta.permlevel_conflict")),
            "{group}: {errors:?}"
        );
    }
}

#[test]
fn the_migrated_form_does_not_loosen_the_tag_rule() {
    // A pii tag with only a permlevel (no group) is refused before and after this change.
    let mut d = Definition::new("doc");
    let mut f = FieldSpec::new("secret_note", FieldType::Text);
    f.classification = vec!["pii".into()];
    f.permlevel = 1;
    d.fields = vec![f];
    let errors = build(vec![d])
        .err()
        .unwrap_or_else(|| panic!("must be refused"));
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("meta.tag_unprotected"))
    );
    // A pii tag in a real group still rebuilds from its own output.
    let mut d = Definition::new("doc");
    let mut f = FieldSpec::new("note", FieldType::Text);
    f.classification = vec!["pii".into()];
    f.protection_group = Some("sensitive".into());
    d.fields = vec![f];
    d.protection_groups = vec![ProtectionGroupSpec::new("sensitive", "Sensitive")];
    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let again = build(m.definitions().to_vec()).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(m.definitions(), again.definitions());
}

#[test]
fn version_defaults_to_revision_plus_one_and_can_be_reproduced() {
    let defs = vec![with_permlevel()];
    let default =
        Registry::build(defs.clone(), &Customisations::new()).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(default.version(), 1);
    let at_zero = Registry::build(defs.clone(), &Customisations::with_revision(0))
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(at_zero.version(), 1);
    let seven = Registry::build(defs.clone(), &Customisations::with_revision(6))
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(seven.version(), 7);
    // Same content at another version, and the follower round trip keeps the agreed version.
    assert_eq!(seven.definitions(), default.definitions());
    let follower = Registry::build(
        seven.definitions().to_vec(),
        &Customisations::with_revision(seven.version() - 1),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(follower.version(), 7);
    assert_eq!(follower.definitions(), seven.definitions());
    // A later change still raises it.
    let mut c = Customisations::with_revision(6);
    c.add_field("invoice", FieldSpec::new("custom_x", FieldType::Text), None);
    let bumped = Registry::build(defs, &c).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(bumped.version(), 8);
}
