use serde_json::json;

use crate::steps::drop_field;
use crate::{Registry, ShapeChange, ShapeChangeKind, Upcaster, check_patch};

fn register(registry: &mut Registry, entity: &str, from: u64, to: u64) {
    registry
        .register(Upcaster::new(entity, from, to, drop_field("legacy")))
        .expect("registered");
}

/// The additive kinds: a change that leaves every existing image readable.
const ADDITIVE: [ShapeChangeKind; 2] = [ShapeChangeKind::DocTypeAdded, ShapeChangeKind::FieldAdded];

/// The kinds that rewrite or remove something an image holds.
const SHAPE_CHANGING: [ShapeChangeKind; 6] = [
    ShapeChangeKind::DocTypeRemoved,
    ShapeChangeKind::FieldRemoved,
    ShapeChangeKind::FieldRenamed,
    ShapeChangeKind::FieldRetyped,
    ShapeChangeKind::FieldRequiredChanged,
    ShapeChangeKind::LinkTargetChanged,
];

#[test]
fn kind_names_follow_the_change_vocabulary() {
    // The spellings `ChangeKind` uses in spot-migration-plan, as the wire form writes them.
    let rows = [
        (ShapeChangeKind::DocTypeAdded, "doc_type_added"),
        (ShapeChangeKind::DocTypeRemoved, "doc_type_removed"),
        (ShapeChangeKind::FieldAdded, "field_added"),
        (ShapeChangeKind::FieldRemoved, "field_removed"),
        (ShapeChangeKind::FieldRenamed, "field_renamed"),
        (ShapeChangeKind::FieldRetyped, "field_retyped"),
        (
            ShapeChangeKind::FieldRequiredChanged,
            "field_required_changed",
        ),
        (ShapeChangeKind::LinkTargetChanged, "link_target_changed"),
    ];
    for (kind, name) in rows {
        assert_eq!(kind.as_str(), name);
        assert_eq!(serde_json::to_value(kind).expect("wire form"), json!(name));
        assert_eq!(
            serde_json::from_value::<ShapeChangeKind>(json!(name)).expect("parsed"),
            kind
        );
    }
}

#[test]
fn only_the_additive_kinds_need_no_upcaster() {
    for kind in ADDITIVE {
        assert!(!kind.needs_upcaster(), "{kind:?} is additive");
    }
    for kind in SHAPE_CHANGING {
        assert!(kind.needs_upcaster(), "{kind:?} changes the stored shape");
    }
}

#[test]
fn additive_changes_pass_an_empty_registry() {
    let registry = Registry::new();
    let changes = [
        ShapeChange::new("party", 1, 2, ShapeChangeKind::FieldAdded),
        ShapeChange::new("party", 1, 2, ShapeChangeKind::DocTypeAdded),
    ];
    assert!(check_patch(&registry, &changes).is_empty());
}

#[test]
fn each_shape_change_without_an_upcaster_is_reported() {
    for kind in SHAPE_CHANGING {
        let registry = Registry::new();
        let findings = check_patch(&registry, &[ShapeChange::new("party", 2, 3, kind)]);
        assert_eq!(findings.len(), 1, "{kind:?}");
        let finding = &findings[0];
        assert_eq!(finding.entity(), "party");
        assert_eq!(finding.kind(), kind);
        assert_eq!(finding.from(), 2);
        assert_eq!(finding.to(), 3);
        assert_eq!(finding.missing_from(), 2);
        assert_eq!(finding.missing_to(), 3);
        let text = finding.to_string();
        assert!(text.contains("party"), "{text}");
        assert!(text.contains(kind.as_str()), "{text}");
    }
}

#[test]
fn a_covered_change_passes() {
    let mut registry = Registry::new();
    register(&mut registry, "party", 2, 3);
    let changes = [ShapeChange::new(
        "party",
        2,
        3,
        ShapeChangeKind::FieldRenamed,
    )];
    assert!(check_patch(&registry, &changes).is_empty());
}

#[test]
fn a_change_covered_by_two_consecutive_steps_passes() {
    let mut registry = Registry::new();
    register(&mut registry, "party", 2, 3);
    register(&mut registry, "party", 3, 4);
    let changes = [ShapeChange::new(
        "party",
        2,
        4,
        ShapeChangeKind::FieldRetyped,
    )];
    assert!(check_patch(&registry, &changes).is_empty());
}

#[test]
fn a_change_covered_only_until_the_middle_step_names_the_gap() {
    let mut registry = Registry::new();
    register(&mut registry, "party", 2, 3);
    let changes = [ShapeChange::new(
        "party",
        2,
        4,
        ShapeChangeKind::FieldRetyped,
    )];
    let findings = check_patch(&registry, &changes);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].missing_from(), 3);
    assert_eq!(findings[0].missing_to(), 4);
}

#[test]
fn another_entitys_step_does_not_cover_this_entity() {
    let mut registry = Registry::new();
    register(&mut registry, "item", 2, 3);
    let changes = [ShapeChange::new(
        "party",
        2,
        3,
        ShapeChangeKind::FieldRenamed,
    )];
    let findings = check_patch(&registry, &changes);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].entity(), "party");
}

#[test]
fn a_change_that_asks_for_no_rewrite_is_not_reported() {
    let registry = Registry::new();
    let changes = [
        ShapeChange::new("party", 3, 3, ShapeChangeKind::FieldRenamed),
        ShapeChange::new("party", 4, 3, ShapeChangeKind::FieldRemoved),
    ];
    assert!(check_patch(&registry, &changes).is_empty());
}

#[test]
fn every_uncovered_change_is_reported_in_order() {
    let mut registry = Registry::new();
    register(&mut registry, "party", 1, 2);
    let changes = [
        ShapeChange::new("party", 1, 2, ShapeChangeKind::FieldRenamed),
        ShapeChange::new("party", 3, 4, ShapeChangeKind::FieldRemoved),
        ShapeChange::new("item", 1, 2, ShapeChangeKind::FieldRetyped),
    ];
    let findings = check_patch(&registry, &changes);
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].entity(), "party");
    assert_eq!(findings[0].from(), 3);
    assert_eq!(findings[1].entity(), "item");
}

#[test]
fn an_upcaster_that_does_not_cover_the_range_is_not_coverage() {
    let mut registry = Registry::new();
    register(&mut registry, "party", 5, 6);
    let changes = [ShapeChange::new(
        "party",
        2,
        3,
        ShapeChangeKind::FieldRenamed,
    )];
    assert_eq!(check_patch(&registry, &changes).len(), 1);
}

#[test]
fn findings_and_changes_are_plain_data() {
    let change = ShapeChange::new("party", 1, 2, ShapeChangeKind::FieldRenamed);
    assert_eq!(change.entity, "party");
    assert_eq!(change.from, 1);
    assert_eq!(change.to, 2);
    assert_eq!(change.kind, ShapeChangeKind::FieldRenamed);
    assert_eq!(change.clone(), change);
}
