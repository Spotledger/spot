//! The expand/migrate/contract classification: one table row per change kind, the two directions
//! of a required toggle, the refused destructive change and the classification of a whole plan.
//!
//! The table is the contract. `classify_change` names every `ChangeKind` with no wildcard arm, so
//! the plan adding a kind is already a compile error there; [`witness`] repeats that match in the
//! tests so a kind added to the table's blind spot is a build failure here too, and
//! [`every_kind_appears_once_in_the_table`] keeps the table itself complete.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::{blocked, change, plan};
use spot_expand_contract::{Phase, classify, classify_change, contract_object};
use spot_migration_plan::risk::rules;
use spot_migration_plan::{ChangeKind, Risk};

/// The number of kinds `ChangeKind` had when this table was written.
///
/// A kind added by `spot-migration-plan` fails [`witness`]'s build first; a kind **removed** from
/// it fails [`every_kind_appears_once_in_the_table`], which points at the stale row. Neither can
/// leave the table quietly incomplete.
const KINDS_WHEN_WRITTEN: usize = 39;

/// Every kind with the rule the plan records for it and the phase it must be classified as.
///
/// Exactly one row per kind, so a kind cannot be left out of the test by accident.
const KINDS: &[(ChangeKind, &str, Option<Phase>)] = &[
    // Expand: the shape grows.
    (
        ChangeKind::SchemaAddEntity,
        rules::SCHEMA_ADD_ENTITY,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::SchemaAddColumn,
        rules::SCHEMA_ADD_COLUMN,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::SchemaAddIndex,
        rules::SCHEMA_ADD_INDEX,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::SchemaWidenType,
        rules::SCHEMA_WIDEN_TYPE,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::SchemaRelaxConstraint,
        rules::SCHEMA_RELAX_CONSTRAINT,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::SchemaAddForeignKey,
        rules::SCHEMA_ADD_FOREIGN_KEY,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::SchemaCleanupIndex,
        rules::SCHEMA_CLEANUP_INDEX,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::SchemaDropIndex,
        rules::SCHEMA_DROP_TRAVERSAL_INDEX,
        Some(Phase::Contract),
    ),
    (
        ChangeKind::SchemaFinalise,
        rules::SCHEMA_FINALISE,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::DocTypeAdded,
        rules::METADATA_DOCTYPE_ADDED,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::FieldAdded,
        rules::METADATA_FIELD_ADDED,
        Some(Phase::Expand),
    ),
    (
        ChangeKind::FieldDefaultChanged,
        rules::METADATA_FIELD_DEFAULT_CHANGED,
        Some(Phase::Expand),
    ),
    // Migrate: data moves into the expanded shape.
    (
        ChangeKind::SchemaBackfill,
        rules::SCHEMA_BACKFILL,
        Some(Phase::Migrate),
    ),
    (
        ChangeKind::SchemaValidate,
        rules::SCHEMA_VALIDATE,
        Some(Phase::Migrate),
    ),
    // Contract: the shape is removed or tightened.
    (
        ChangeKind::SchemaBlocked,
        rules::SCHEMA_BLOCKED,
        Some(Phase::Contract),
    ),
    (
        ChangeKind::SchemaSetNotNull,
        rules::SCHEMA_SET_NOT_NULL,
        Some(Phase::Contract),
    ),
    (
        ChangeKind::DocTypeRemoved,
        rules::METADATA_DOCTYPE_REMOVED,
        Some(Phase::Contract),
    ),
    (
        ChangeKind::FieldRemoved,
        rules::METADATA_FIELD_REMOVED,
        Some(Phase::Contract),
    ),
    (
        ChangeKind::FieldRetyped,
        rules::METADATA_FIELD_RETYPED,
        Some(Phase::Contract),
    ),
    (
        ChangeKind::FieldRenamed,
        rules::METADATA_FIELD_RENAMED,
        Some(Phase::Contract),
    ),
    (
        ChangeKind::LinkTargetChanged,
        rules::METADATA_LINK_TARGET_CHANGED,
        Some(Phase::Contract),
    ),
    // One kind, two directions: the table lists the contract one, the expand one is asserted
    // separately.
    (
        ChangeKind::FieldRequiredChanged,
        rules::METADATA_FIELD_REQUIRED_SET,
        Some(Phase::Contract),
    ),
    // No phase: metadata that publishes without removing a storage object.
    (
        ChangeKind::NamingChanged,
        rules::METADATA_NAMING_CHANGED,
        None,
    ),
    (
        ChangeKind::ProtectionChanged,
        rules::METADATA_PROTECTION_CHANGED,
        None,
    ),
    (
        ChangeKind::PermissionRuleAdded,
        rules::METADATA_PERMISSION_ADDED,
        None,
    ),
    (
        ChangeKind::PermissionRuleRemoved,
        rules::METADATA_PERMISSION_REMOVED,
        None,
    ),
    (
        ChangeKind::ViewLayerChanged,
        rules::METADATA_VIEW_CHANGED,
        None,
    ),
    // No phase: patches.
    (ChangeKind::PatchAdded, rules::PATCH_ADDED, None),
    (ChangeKind::PatchMissing, rules::PATCH_MISSING, None),
    // No phase: behaviour.
    (ChangeKind::StepAdded, rules::BEHAVIOUR_STEP_ADDED, None),
    (ChangeKind::StepRemoved, rules::BEHAVIOUR_STEP_REMOVED, None),
    (
        ChangeKind::StepReordered,
        rules::BEHAVIOUR_STEP_REORDERED,
        None,
    ),
    (
        ChangeKind::StepReprioritised,
        rules::BEHAVIOUR_STEP_REPRIORITISED,
        None,
    ),
    (ChangeKind::MethodAdded, rules::BEHAVIOUR_METHOD_ADDED, None),
    (
        ChangeKind::MethodRemoved,
        rules::BEHAVIOUR_METHOD_REMOVED,
        None,
    ),
    (
        ChangeKind::MethodSignatureChanged,
        rules::BEHAVIOUR_METHOD_SIGNATURE_CHANGED,
        None,
    ),
    (
        ChangeKind::ScheduleChanged,
        rules::BEHAVIOUR_SCHEDULE_CHANGED,
        None,
    ),
    (
        ChangeKind::CapabilityWidened,
        rules::BEHAVIOUR_CAPABILITY_WIDENED,
        None,
    ),
    (
        ChangeKind::SeamOverrideChanged,
        rules::BEHAVIOUR_SEAM_OVERRIDE_CHANGED,
        None,
    ),
];

/// A stable name per kind, from an exhaustive match with **no** wildcard arm.
///
/// This is the build-time guard. `ChangeKind` lives in another crate and exposes no `ALL`
/// iterator, so a table alone cannot notice a new variant; this match can. When
/// `spot-migration-plan` adds a kind, this file stops compiling with the missing variant named,
/// and the phase table has to grow with it.
fn witness(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::SchemaAddEntity => "schema_add_entity",
        ChangeKind::SchemaAddColumn => "schema_add_column",
        ChangeKind::SchemaAddIndex => "schema_add_index",
        ChangeKind::SchemaWidenType => "schema_widen_type",
        ChangeKind::SchemaRelaxConstraint => "schema_relax_constraint",
        ChangeKind::SchemaAddForeignKey => "schema_add_foreign_key",
        ChangeKind::SchemaValidate => "schema_validate",
        ChangeKind::SchemaCleanupIndex => "schema_cleanup_index",
        ChangeKind::SchemaDropIndex => "schema_drop_index",
        ChangeKind::SchemaFinalise => "schema_finalise",
        ChangeKind::SchemaBackfill => "schema_backfill",
        ChangeKind::SchemaSetNotNull => "schema_set_not_null",
        ChangeKind::SchemaBlocked => "schema_blocked",
        ChangeKind::DocTypeAdded => "doctype_added",
        ChangeKind::DocTypeRemoved => "doctype_removed",
        ChangeKind::FieldAdded => "field_added",
        ChangeKind::FieldRemoved => "field_removed",
        ChangeKind::FieldRetyped => "field_retyped",
        ChangeKind::FieldRenamed => "field_renamed",
        ChangeKind::FieldRequiredChanged => "field_required_changed",
        ChangeKind::FieldDefaultChanged => "field_default_changed",
        ChangeKind::NamingChanged => "naming_changed",
        ChangeKind::LinkTargetChanged => "link_target_changed",
        ChangeKind::ProtectionChanged => "protection_changed",
        ChangeKind::PermissionRuleAdded => "permission_rule_added",
        ChangeKind::PermissionRuleRemoved => "permission_rule_removed",
        ChangeKind::ViewLayerChanged => "view_layer_changed",
        ChangeKind::PatchAdded => "patch_added",
        ChangeKind::PatchMissing => "patch_missing",
        ChangeKind::StepAdded => "step_added",
        ChangeKind::StepRemoved => "step_removed",
        ChangeKind::StepReordered => "step_reordered",
        ChangeKind::StepReprioritised => "step_reprioritised",
        ChangeKind::MethodAdded => "method_added",
        ChangeKind::MethodRemoved => "method_removed",
        ChangeKind::MethodSignatureChanged => "method_signature_changed",
        ChangeKind::ScheduleChanged => "schedule_changed",
        ChangeKind::CapabilityWidened => "capability_widened",
        ChangeKind::SeamOverrideChanged => "seam_override_changed",
    }
}

fn phase_of(kind: ChangeKind, rule: &str) -> Option<Phase> {
    classify_change(&change("c", kind, rule, "party.note", Risk::Additive))
}

#[test]
fn every_kind_appears_once_in_the_table() {
    let mut names: Vec<&'static str> = KINDS.iter().map(|row| witness(row.0)).collect();
    names.sort_unstable();
    let listed = names.len();
    names.dedup();
    assert_eq!(
        names.len(),
        listed,
        "a kind is listed twice in KINDS: {names:?}"
    );
    assert_eq!(
        KINDS.len(),
        KINDS_WHEN_WRITTEN,
        "ChangeKind has more or fewer kinds than this table: add or remove the row (and the \
         witness arm) for every kind the plan defines"
    );
    assert_eq!(
        names.len(),
        KINDS_WHEN_WRITTEN,
        "KINDS does not name every kind: {names:?}"
    );
}

#[test]
fn every_kind_classifies_as_its_table_says() {
    for &(kind, rule, expected) in KINDS {
        assert_eq!(
            phase_of(kind, rule),
            expected,
            "{} with rule {rule}",
            witness(kind)
        );
    }
}

#[test]
fn making_a_field_required_contracts_and_clearing_it_expands() {
    assert_eq!(
        phase_of(
            ChangeKind::FieldRequiredChanged,
            rules::METADATA_FIELD_REQUIRED_SET
        ),
        Some(Phase::Contract)
    );
    assert_eq!(
        phase_of(
            ChangeKind::FieldRequiredChanged,
            rules::METADATA_FIELD_REQUIRED_CLEARED
        ),
        Some(Phase::Expand)
    );
}

#[test]
fn classify_a_plan_in_change_order() {
    let release = plan(vec![
        change(
            "schema/add_column:party.note",
            ChangeKind::SchemaAddColumn,
            rules::SCHEMA_ADD_COLUMN,
            "party.note",
            Risk::Additive,
        ),
        change(
            "schema/backfill:party.note",
            ChangeKind::SchemaBackfill,
            rules::SCHEMA_BACKFILL,
            "party.note",
            Risk::NeedsReview,
        ),
        blocked("party.legacy"),
    ]);
    let classified = classify(&release);
    assert_eq!(classified.len(), 3);
    assert_eq!(classified[0].phase, Phase::Expand);
    assert_eq!(
        classified[0].object.as_ref().unwrap().to_string(),
        "party.note"
    );
    assert_eq!(classified[1].phase, Phase::Migrate);
    assert_eq!(classified[2].phase, Phase::Contract);
    assert_eq!(
        classified[2].object.as_ref().unwrap().to_string(),
        "party.legacy"
    );
    assert!(classified[2].phase.is_contract());
    assert_eq!(classified[2].risk, Risk::Destructive);
}

#[test]
fn a_rename_contracts_the_previous_name() {
    let renamed = change(
        "metadata/party/field_renamed/new_name",
        ChangeKind::FieldRenamed,
        rules::METADATA_FIELD_RENAMED,
        "party.new_name",
        Risk::Destructive,
    )
    .evidence("old_name", "new_name");
    assert_eq!(
        contract_object(&renamed).unwrap().to_string(),
        "party.old_name"
    );
    assert_eq!(classify_change(&renamed), Some(Phase::Contract));
}

#[test]
fn a_table_target_parses_as_a_table() {
    let removed = change(
        "metadata/party/doctype_removed",
        ChangeKind::DocTypeRemoved,
        rules::METADATA_DOCTYPE_REMOVED,
        "party",
        Risk::Destructive,
    );
    let object = contract_object(&removed).expect("an object");
    assert!(object.is_table());
    assert_eq!(object.to_string(), "party");
}
