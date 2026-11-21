//! The script checks: the acceptance case (a script referencing a removed field appears in the
//! plan), a syntax error, a removed DocType, and no false positive for a script that reads a
//! field the release keeps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{definition, field, meta, release, system};
use spot_meta::{Customisations, FieldType};
use spot_migration_conflicts::{
    ArtefactKind, ConflictCategory, ReleaseContext, SiteScript, codes, detect,
};
use spot_migration_plan::ChangeKind;

/// A `party` with a `name` and an `internal_note`, then one without the note.
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
fn a_script_reading_a_removed_field_appears_in_the_plan() {
    let (new_standard, plan) = party_without_note();
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let scripts = vec![SiteScript {
        name: "nightly_rollup".to_owned(),
        app: "erp_selling".to_owned(),
        doctype: Some("party".to_owned()),
        hook_point: "validate_save".to_owned(),
        source: "export function on_validate(doc: Party) { return doc.internal_note; }".to_owned(),
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_scripts(&scripts);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::SCRIPT_TYPE_ERROR);
    assert_eq!(item.category, ConflictCategory::ScriptTypeError);
    assert_eq!(item.artefacts.len(), 1);
    assert_eq!(item.artefacts[0].kind, ArtefactKind::Script);
    assert_eq!(item.artefacts[0].id, "erp_selling:nightly_rollup");
    assert_eq!(
        item.cause.release_item,
        "metadata/party/field_removed/internal_note"
    );
    assert!(item.cause.effect.contains("internal_note"));

    // The conflict converts to a plan change bound to the same identity and risk.
    let change = item.to_change();
    assert_eq!(change.kind, ChangeKind::StepRemoved);
    assert_eq!(change.rule, "conflict/script_type_error");
    assert_eq!(change.id, item.id);
    assert_eq!(report.to_plan_changes().len(), 1);
}

#[test]
fn a_script_that_reads_a_kept_field_is_not_a_conflict() {
    let (new_standard, plan) = party_without_note();
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let scripts = vec![SiteScript {
        name: "greeting".to_owned(),
        app: "erp_selling".to_owned(),
        doctype: Some("party".to_owned()),
        hook_point: "validate_save".to_owned(),
        source: "export function on_validate(doc: Party) { return doc.name; }".to_owned(),
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_scripts(&scripts);
    assert!(detect(&context).is_empty());
}

#[test]
fn a_script_reading_a_renamed_field_says_which_name_to_use() {
    let from = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("note", FieldType::Text),
        ],
    );
    let to = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("internal_note", FieldType::Text),
        ],
    );
    let mut rename_system = system(std::slice::from_ref(&to), 2);
    rename_system.rename_hints = vec![spot_migration_plan::RenameHint {
        doctype: "party".to_owned(),
        from: "note".to_owned(),
        to: "internal_note".to_owned(),
    }];
    let plan = release(&system(&[from], 1), &rename_system);
    let new_standard = vec![to];
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let scripts = vec![SiteScript {
        name: "rollup".to_owned(),
        app: "erp_selling".to_owned(),
        doctype: Some("party".to_owned()),
        hook_point: "validate_save".to_owned(),
        source: "export function on_validate(doc: Party) { return doc.note; }".to_owned(),
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_scripts(&scripts);
    let report = detect(&context);
    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::SCRIPT_TYPE_ERROR);
    assert!(item.resolution.contains("internal_note"));
}

#[test]
fn a_script_that_does_not_transpile_is_reported() {
    let new_standard = vec![definition("party", vec![field("name", FieldType::Text)])];
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let plan = release(&system(&new_standard, 1), &system(&new_standard, 1));
    let scripts = vec![SiteScript {
        name: "broken".to_owned(),
        app: "erp_selling".to_owned(),
        doctype: Some("party".to_owned()),
        hook_point: "validate_save".to_owned(),
        source: "export function on_validate(doc: Party) { return doc. ; }".to_owned(),
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_scripts(&scripts);
    let report = detect(&context);
    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::SCRIPT_SYNTAX_ERROR);
}

#[test]
fn a_script_bound_to_a_removed_doctype_is_reported() {
    let from = vec![
        definition("party", vec![field("name", FieldType::Text)]),
        definition("legacy_note", vec![field("text", FieldType::Text)]),
    ];
    let to = vec![definition("party", vec![field("name", FieldType::Text)])];
    let plan = release(&system(&from, 1), &system(&to, 2));
    let new_standard = to;
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let scripts = vec![SiteScript {
        name: "note_guard".to_owned(),
        app: "erp_selling".to_owned(),
        doctype: Some("legacy_note".to_owned()),
        hook_point: "validate_save".to_owned(),
        source: "export function on_validate(doc: LegacyNote) { return doc.text; }".to_owned(),
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_scripts(&scripts);
    let report = detect(&context);
    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::SCRIPT_DOCTYPE_REMOVED);
}
