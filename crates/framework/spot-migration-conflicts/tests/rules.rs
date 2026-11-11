//! The rule and expression checks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{definition, field, meta, release, system};
use spot_meta::{Customisations, FieldType, Property};
use spot_migration_conflicts::{ArtefactKind, ReleaseContext, SiteRule, codes, detect};

fn party_without_note() -> (Vec<spot_meta::Definition>, spot_migration_plan::ReleasePlan) {
    let from = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("internal_note", FieldType::Text),
        ],
    );
    let to = definition("party", vec![field("name", FieldType::Text)]);
    let new_standard = vec![to.clone()];
    let plan = release(&system(&[from], 1), &system(&[to], 2));
    (new_standard, plan)
}

#[test]
fn a_rule_reading_a_removed_field_is_reported() {
    let (new_standard, plan) = party_without_note();
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let rules = vec![SiteRule {
        name: "vip_only".to_owned(),
        doctype: Some("party".to_owned()),
        expression: "doc.internal_note == 'vip'".to_owned(),
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_rules(&rules);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::RULE_FIELD_REMOVED);
    assert_eq!(item.artefacts[0].kind, ArtefactKind::Rule);
    assert_eq!(item.artefacts[0].id, "vip_only");
}

#[test]
fn a_rule_that_no_longer_parses_is_reported() {
    let (new_standard, plan) = party_without_note();
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let rules = vec![SiteRule {
        name: "broken".to_owned(),
        doctype: Some("party".to_owned()),
        expression: "doc.name ==".to_owned(),
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_rules(&rules);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::RULE_PARSE_ERROR);
}

#[test]
fn an_override_expression_reading_a_removed_field_is_reported() {
    let (new_standard, plan) = party_without_note();
    let mut customisations = Customisations::new();
    customisations.set(
        "party",
        Some("name"),
        Property::DependsOn(Some("doc.internal_note == 'x'".to_owned())),
    );
    let meta = meta(&new_standard, &Customisations::new());
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::RULE_FIELD_REMOVED);
    assert_eq!(item.artefacts[0].id, "override:party.name#depends_on");
}

#[test]
fn a_rule_the_release_leaves_alone_is_not_a_conflict() {
    let definitions = vec![definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("internal_note", FieldType::Text),
        ],
    )];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let customisations = Customisations::new();
    let meta = meta(&definitions, &customisations);
    let rules = vec![SiteRule {
        name: "vip_only".to_owned(),
        doctype: Some("party".to_owned()),
        expression: "doc.internal_note == 'vip'".to_owned(),
    }];
    let context =
        ReleaseContext::new(&plan, &definitions, &meta, &customisations).with_rules(&rules);
    assert!(detect(&context).is_empty());
}
