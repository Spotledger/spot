//! Risk, estimate, determinism, the apply steps and `verify_applied`.
#![allow(missing_docs)]

mod common;

use std::collections::BTreeMap;

use common::{SCENARIOS, dt, field, scenario, step, system};
use spot_meta::FieldType;
use spot_migration_plan::{
    Change, ChangeKind, CostModel, ReleasePlan, Risk, RiskPolicy, RiskTable, SchemaPlan,
    SchemaStepKind, SystemDescription, TableStat, TableStats, estimate, plan, plan_with,
    risk::rules, verify_applied,
};

/// Reverses every list in a system, to prove the plan does not depend on input order.
fn shuffled(mut system: SystemDescription) -> SystemDescription {
    system.apps.reverse();
    for app in &mut system.apps {
        app.manifest.events.reverse();
        app.manifest.schedules.reverse();
        app.manifest.patches.reverse();
        app.manifest.overrides.reverse();
        app.manifest.capabilities.network.reverse();
        app.manifest.capabilities.doctypes_read.reverse();
        app.manifest.capabilities.doctypes_write.reverse();
    }
    system.doctypes.reverse();
    for dt in &mut system.doctypes {
        dt.definition.fields.reverse();
        dt.permissions.reverse();
    }
    system.methods.reverse();
    system.seam_steps.reverse();
    system.applied_patches.reverse();
    system.rename_hints.reverse();
    system
}

/// The first change of a kind, or a panic naming the kind.
fn change_of_kind(release: &ReleasePlan, kind: ChangeKind) -> &Change {
    match release.changes.iter().find(|c| c.kind == kind) {
        Some(change) => change,
        None => panic!("no change of kind {kind:?}"),
    }
}

/// A parsed value or a panic with the error.
fn expect_ok<T, E: std::fmt::Display>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{error}"),
    }
}

/// The only change of the estimate, or a panic.
fn only_change(estimate: &spot_migration_plan::Estimate) -> &spot_migration_plan::ChangeEstimate {
    match estimate.per_change.first() {
        Some(change) if estimate.per_change.len() == 1 => change,
        other => panic!("expected exactly one change, got {other:?}"),
    }
}

#[test]
fn shuffled_input_gives_the_same_digest() {
    for name in SCENARIOS {
        let s = scenario(name);
        let a = plan(&s.from, &s.to, &s.schema);
        let b = plan(&shuffled(s.from), &shuffled(s.to), &s.schema);
        assert_eq!(
            a.plan_digest, b.plan_digest,
            "{name}: digest depends on order"
        );
        assert_eq!(a.changes, b.changes, "{name}: changes depend on order");
        assert_eq!(a.json(), b.json(), "{name}: JSON depends on order");
    }
}

#[test]
fn a_site_policy_can_raise_a_risk_but_not_lower_it() {
    let s = scenario("add_field");
    let builtin = plan(&s.from, &s.to, &s.schema);
    assert_eq!(
        change_of_kind(&builtin, ChangeKind::FieldAdded).risk,
        Risk::Additive
    );

    // Raise an additive rule to destructive.
    let raised = RiskTable::builtin()
        .with_policy(&RiskPolicy::new().raise(rules::METADATA_FIELD_ADDED, Risk::Destructive));
    let raised_plan = plan_with(&s.from, &s.to, &s.schema, &raised, &CostModel::default());
    assert_eq!(
        change_of_kind(&raised_plan, ChangeKind::FieldAdded).risk,
        Risk::Destructive
    );
    assert_eq!(raised_plan.risk, Risk::Destructive);

    // A policy that tries to lower a destructive rule is ignored.
    let lowered = RiskTable::builtin()
        .with_policy(&RiskPolicy::new().raise(rules::METADATA_FIELD_REMOVED, Risk::Additive));
    assert_eq!(
        lowered.risk_of(rules::METADATA_FIELD_REMOVED),
        Risk::Destructive
    );
}

#[test]
fn every_review_or_destructive_change_has_a_reason() {
    for name in SCENARIOS {
        let s = scenario(name);
        let release = plan(&s.from, &s.to, &s.schema);
        for change in &release.changes {
            if change.risk != Risk::Additive {
                assert!(
                    change.reason.is_some(),
                    "{name}: {} ({:?}) has no reason",
                    change.id,
                    change.kind
                );
            }
        }
    }
}

#[test]
fn the_risk_table_loads_from_toml() {
    let table = expect_ok(RiskTable::from_toml(
        r#"
        [rules]
        "metadata/field_added" = "needs_review"
        "#,
    ));
    assert_eq!(
        table.risk_of(rules::METADATA_FIELD_ADDED),
        Risk::NeedsReview
    );
    // A rule the file does not name keeps its compiled-in default.
    assert_eq!(
        table.risk_of(rules::METADATA_FIELD_REMOVED),
        Risk::Destructive
    );
    // An unknown rule is never silently additive.
    assert_eq!(table.risk_of("not/a_rule"), Risk::NeedsReview);
    // A file cannot lower a compiled-in risk.
    let lowered = expect_ok(RiskTable::from_toml(
        r#"
        [rules]
        "metadata/field_removed" = "additive"
        "#,
    ));
    assert_eq!(
        lowered.risk_of(rules::METADATA_FIELD_REMOVED),
        Risk::Destructive
    );
    assert!(RiskTable::from_toml("rules = 3").is_err());
}

/// A system whose only change is a backfill of `party.note`, with `rows` statistics.
fn backfill(rows: Option<u64>) -> (SystemDescription, SystemDescription, SchemaPlan, TableStats) {
    let from = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    let to = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    let schema = common::schema_steps(vec![step(
        "backfill:party.note",
        SchemaStepKind::Backfill,
        "party",
        Some("note"),
    )]);
    let mut tables = BTreeMap::new();
    tables.insert("party".to_owned(), TableStat { rows, bytes: None });
    (from, to, schema, TableStats { tables })
}

#[test]
fn estimates_one_row_and_fifty_million_rows() {
    let (from, to, schema, stats) = backfill(Some(1));
    let release = plan(&from, &to, &schema);
    let est = estimate(&release, &stats, &CostModel::default());
    let change = only_change(&est);
    assert_eq!(change.expected, 1, "one row takes one second at the floor");
    assert!(!change.unknown);
    assert_eq!(change.rows, Some(1));

    let (from, to, schema, stats) = backfill(Some(50_000_000));
    let release = plan(&from, &to, &schema);
    let est = estimate(&release, &stats, &CostModel::default());
    let change = only_change(&est);
    // 50,000,000 rows at 5,000 rows/s is 10,000 seconds.
    assert_eq!(change.expected, 10_000);
    assert_eq!(change.high, 20_000);
    assert_eq!(change.low, 5_000);
    assert_eq!(change.rows_per_second, Some(5_000));
}

#[test]
fn unknown_statistics_widen_the_range() {
    let (from, to, schema, stats) = backfill(None);
    let release = plan(&from, &to, &schema);
    let est = estimate(&release, &stats, &CostModel::default());
    assert!(est.unknown);
    let change = only_change(&est);
    assert!(change.unknown);
    assert_eq!(change.low, 0, "an unknown estimate does not guess zero");
    assert_eq!(change.expected, 200, "the fallback row count is 1,000,000");
    assert_eq!(change.high, 2_000, "the high bound widens by the factor");
    assert!(change.rows.is_none());
}

#[test]
fn the_cost_model_loads_from_toml() {
    let model = expect_ok(CostModel::from_toml("backfill_rows_per_second = 2000\n"));
    assert_eq!(model.backfill_rows_per_second, 2_000);
    assert_eq!(
        model.index_rows_per_second, 50_000,
        "absent keys keep defaults"
    );
    assert!(CostModel::from_toml("backfill_rows_per_second = 0").is_err());
}

#[test]
fn verify_applied_finds_a_seeded_divergence() {
    let s = scenario("add_field");
    let release = plan(&s.from, &s.to, &s.schema);

    // The result is exactly the target: no mismatches.
    let clean = verify_applied(&release, &s.to);
    assert!(clean.is_empty(), "clean result: {clean:?}");

    // The field the plan adds is missing from the result: a divergence.
    let mut after = s.to.clone();
    after.doctypes[0]
        .definition
        .fields
        .retain(|f| f.name != "note");
    let diverged = verify_applied(&release, &after);
    assert!(!diverged.is_empty());
    assert!(
        diverged
            .mismatches
            .iter()
            .any(|c| c.kind == ChangeKind::FieldRemoved),
        "the missing field appears: {diverged:?}"
    );
}

#[test]
fn apply_steps_are_ordered_and_identified() {
    use spot_migration_plan::ApplyStepKind;
    let s = scenario("add_field");
    let release = plan(&s.from, &s.to, &s.schema);
    let steps = release.steps();
    let kinds: Vec<ApplyStepKind> = steps.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        vec![ApplyStepKind::Schema, ApplyStepKind::PublishMetadata]
    );
    for step in &steps {
        assert!(step.idempotency_key.starts_with(&release.plan_digest));
        assert!(step.idempotency_key.ends_with(&step.id));
    }
}

#[test]
fn a_downgrade_is_destructive() {
    let s = scenario("downgrade");
    let release = plan(&s.from, &s.to, &s.schema);
    assert_eq!(release.risk, Risk::Destructive);
    assert_eq!(
        change_of_kind(&release, ChangeKind::PatchMissing).risk,
        Risk::Destructive
    );
}

#[test]
fn a_narrowed_decimal_is_blocked() {
    let s = scenario("narrow_blocked");
    let release = plan(&s.from, &s.to, &s.schema);
    assert_eq!(release.risk, Risk::Destructive);
    assert_eq!(
        change_of_kind(&release, ChangeKind::SchemaBlocked).risk,
        Risk::Destructive
    );
    assert_eq!(
        release.estimate.blocking,
        spot_migration_plan::BlockingClass::MaintenanceWindow
    );
}
