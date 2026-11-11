//! The extra-table hook (WP-STO-12.T3): `PlanOptions::extra_tables`. Tests that need a server
//! are skipped, with a SKIPPED line, when `SPOT_TEST_PG_URL` is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_pg_migrate::{
    ApplyOptions, ExtraTable, LiveSchema, MigrateError, PlanOptions, apply, codes, dry_run,
    plan_with,
};
use support::*;

fn extra(table: &str, sql: &[&str]) -> ExtraTable {
    ExtraTable::new(table, sql.iter().map(|s| (*s).to_owned()).collect()).unwrap()
}

fn with(extras: Vec<ExtraTable>) -> PlanOptions {
    PlanOptions {
        extra_tables: extras,
        ..PlanOptions::default()
    }
}

fn table_a() -> ExtraTable {
    extra(
        "spot_x_a",
        &["CREATE TABLE IF NOT EXISTS spot_x_a (id bigint PRIMARY KEY, note text)"],
    )
}

fn index_of_a() -> ExtraTable {
    extra(
        "spot_x_a_note_ix",
        &["CREATE INDEX IF NOT EXISTS spot_x_a_note_ix ON spot_x_a (note)"],
    )
}

fn parties(version: u64) -> spot_storage_contract::Catalog {
    catalog(
        version,
        vec![doc("party", vec![field("title", text(None), false)])],
    )
}

fn keys(plan: &spot_pg_migrate::MigrationPlan) -> Vec<&str> {
    plan.steps.iter().map(|s| s.key.as_str()).collect()
}

/// Pinned from the output of the code before the hook existed: a plan without extras hashes as
/// it always did. Needs no server.
#[test]
fn a_plan_without_extras_keeps_its_golden_hash() {
    let plan = plan_with(&LiveSchema::default(), &parties(1), &PlanOptions::default());
    assert_eq!(keys(&plan), ["create_table:party", "finalise"]);
    assert_eq!(
        plan.plan_hash,
        "3f5e3a42d6163d160dec406fa117540b7ef15b0ff98e0cfbffbbf42f7ef71df2"
    );
}

#[test]
fn the_hash_is_stable_between_runs_and_changes_with_a_statement() {
    let h = |o: &PlanOptions| plan_with(&LiveSchema::default(), &parties(1), o).plan_hash;
    let one = with(vec![table_a(), index_of_a()]);
    assert_eq!(h(&one), h(&one.clone()));
    let other = with(vec![
        extra(
            "spot_x_a",
            &["CREATE TABLE IF NOT EXISTS spot_x_a (id bigint PRIMARY KEY, note text, n int)"],
        ),
        index_of_a(),
    ]);
    assert_ne!(h(&one), h(&other));
}

#[tokio::test]
async fn an_extra_table_is_created_once_and_the_second_apply_plans_nothing() {
    let Some(rig) = rig().await else { return };
    let options = with(vec![table_a()]);
    let wanted = parties(1);
    let plan = dry_run(&rig.backend, &wanted, &options).await.unwrap();
    assert_eq!(
        keys(&plan),
        ["create_table:party", "extra_table:spot_x_a", "finalise"]
    );
    let report = apply(&rig.backend, &plan, ApplyOptions::default())
        .await
        .unwrap();
    assert_eq!(report.executed, keys(&plan));
    assert!(rig.live().await.tables.contains_key("spot_x_a"));

    let again = dry_run(&rig.backend, &wanted, &options).await.unwrap();
    assert!(again.is_empty(), "{:?}", keys(&again));
    let report = apply(&rig.backend, &again, ApplyOptions::default())
        .await
        .unwrap();
    assert!(report.executed.is_empty());
}

#[tokio::test]
async fn extras_run_after_a_document_change_and_an_extra_can_index_an_earlier_one() {
    let Some(rig) = rig().await else { return };
    rig.migrate(&parties(1)).await;
    // A document change (a new column) and two extras in one plan.
    let wanted = catalog(
        2,
        vec![doc(
            "party",
            vec![
                field("title", text(None), false),
                field("note", text(None), true),
            ],
        )],
    );
    let options = with(vec![table_a(), index_of_a()]);
    let plan = dry_run(&rig.backend, &wanted, &options).await.unwrap();
    assert_eq!(
        keys(&plan),
        [
            "add_column:party.note",
            "extra_table:spot_x_a",
            "extra_table:spot_x_a_note_ix",
            "finalise"
        ]
    );
    apply(&rig.backend, &plan, ApplyOptions::default())
        .await
        .unwrap();
    let live = rig.live().await;
    assert!(live.tables.contains_key("spot_x_a"));
    assert!(live.indexes.contains_key("spot_x_a_note_ix"));
    assert!(
        dry_run(&rig.backend, &wanted, &options)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_failing_extra_step_fails_the_apply_keeps_earlier_steps_and_leaks_no_sql() {
    let Some(rig) = rig().await else { return };
    let bad = extra(
        "spot_x_bad",
        &["CREATE TABLE IF NOT EXISTS spot_x_bad (a nosuchtype DEFAULT 'sekret_value')"],
    );
    let later = extra(
        "spot_x_later",
        &["CREATE TABLE IF NOT EXISTS spot_x_later (id bigint PRIMARY KEY)"],
    );
    let options = with(vec![table_a(), bad, later]);
    let plan = dry_run(&rig.backend, &parties(1), &options).await.unwrap();
    let err = apply(&rig.backend, &plan, ApplyOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), codes::DATABASE);
    let text = err.to_string();
    assert!(text.contains("extra_table:spot_x_bad"), "{text}");
    assert!(
        !text.contains("sekret_value") && !text.contains("nosuchtype"),
        "{text}"
    );
    let live = rig.live().await;
    assert!(
        live.tables.contains_key("party"),
        "earlier document step stays"
    );
    assert!(live.tables.contains_key("spot_x_a"), "earlier extra stays");
    assert!(!live.tables.contains_key("spot_x_bad"));
    assert!(
        !live.tables.contains_key("spot_x_later"),
        "later step did not run"
    );
}

#[tokio::test]
async fn every_validation_refusal_changes_nothing() {
    let Some(rig) = rig().await else { return };
    let literal = |table: &str, sql: &[&str]| ExtraTable {
        table: table.to_owned(),
        create: sql.iter().map(|s| (*s).to_owned()).collect(),
    };
    let cases = [
        literal("other_x", &["CREATE TABLE IF NOT EXISTS other_x (a int)"]),
        literal("spot_ok", &["CREATE TABLE IF NOT EXISTS other_y (a int)"]),
        literal("spot_ok", &[]),
        literal("spot_ok", &["DROP TABLE party"]),
        literal("spot_ok", &["ALTER TABLE party ADD COLUMN z int"]),
        literal("spot_ok", &["GRANT ALL ON party TO public"]),
        literal("spot_ok", &["TRUNCATE party"]),
        literal("spot_ok", &["DELETE FROM party"]),
        literal("spot_ok", &["UPDATE party SET title = 'x'"]),
        literal(
            "spot_ok",
            &["CREATE TABLE IF NOT EXISTS spot_ok (a int); DROP TABLE party"],
        ),
        literal(
            "spot_ok",
            &["CREATE INDEX IF NOT EXISTS spot_ok ON party (title)"],
        ),
        // The name of a document table of the catalogue.
        literal("party", &["CREATE TABLE IF NOT EXISTS party (a int)"]),
    ];
    for case in cases {
        let options = with(vec![case.clone()]);
        let plan = dry_run(&rig.backend, &parties(1), &options).await.unwrap();
        assert_eq!(plan.refused.len(), 1, "{case:?}");
        assert_eq!(plan.refused[0].code, codes::EXTRA_TABLE_INVALID, "{case:?}");
        let err = apply(&rig.backend, &plan, ApplyOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, MigrateError::Refused(_)), "{case:?}");
        assert!(
            rig.live().await.tables.is_empty(),
            "nothing is applied: {case:?}"
        );
    }
}

#[tokio::test]
async fn the_advisory_lock_serialises_two_applies_with_extras() {
    let Some(rig) = rig().await else { return };
    let options = with(vec![table_a(), index_of_a()]);
    let wanted = parties(1);
    let first = dry_run(&rig.backend, &wanted, &options).await.unwrap();
    let second = dry_run(&rig.backend, &wanted, &options).await.unwrap();
    let (a, b) = tokio::join!(
        apply(&rig.backend, &first, ApplyOptions::default()),
        apply(&rig.backend, &second, ApplyOptions::default())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let ran = |r: &spot_pg_migrate::ApplyReport| {
        r.executed
            .iter()
            .filter(|k| k.starts_with("extra_table:"))
            .count()
    };
    assert_eq!(ran(&a) + ran(&b), 2, "created once in total: {a:?} {b:?}");
    assert!(
        ran(&a) == 0 || ran(&b) == 0,
        "one of them found nothing to do"
    );
}
