//! Extensions and exclusion constraints against a real PostgreSQL (ADR-087 sections 4.3, 5.2,
//! 5.3 and 5.7; skipped, with a SKIPPED line, when `SPOT_TEST_PG_URL` is unset).
//!
//! `btree_gist`, `pg_trgm` and `unaccent` are contrib modules and can be created in the scratch
//! database. PostGIS is not assumed: the geo path is covered by the planner's unit tests.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_pg_migrate::{
    ApplyOptions, MigrateError, PlanOptions, Risk, Run, apply, codes, dry_run, extension_step,
};
use spot_storage_contract::{
    Catalog, ExclusionDef, FieldType, IndexDef, IndexMethod, OpClass, RangeOf,
};
use support::*;

/// `booking(unit, during)` with an overlap exclusion scoped by `scope`.
fn bookings(version: u64, scope: &[&str], indexes: Vec<IndexDef>) -> Catalog {
    let mut e = doc(
        "booking",
        vec![
            field("unit", text(None), false),
            field(
                "during",
                FieldType::Range {
                    of: RangeOf::DateTime,
                },
                true,
            ),
        ],
    );
    e.indexes = indexes;
    e.exclusions = vec![ExclusionDef {
        name: n("unit_during"),
        field: n("during"),
        scope: scope.iter().map(|s| n(s)).collect(),
        message: "That unit is already booked for these dates.".into(),
    }];
    catalog(version, vec![e])
}

/// The same table without its exclusion, to put rows in first.
fn bookings_plain(version: u64) -> Catalog {
    let mut c = bookings(version, &["unit"], vec![]);
    let mut e = c.entities().next().unwrap().clone();
    e.exclusions = vec![];
    c = catalog(version, vec![e]);
    c
}

fn gist_on_during() -> IndexDef {
    IndexDef {
        name: n("during"),
        fields: vec![n("during")],
        unique: false,
        method: IndexMethod::Gist,
        opclass: None,
        include: vec![],
    }
}

/// Applies `wanted` up to, and not including, the last step (`finalise`, the backend's
/// `prepare`). `spot-postgres` refuses geo and range fields until WP-STO-03.T7 teaches it the
/// column types, so the whole `apply` cannot succeed on a catalogue that holds one yet; every
/// other step runs through the real `apply`, locks, timeouts and pre-flight included. Returns
/// the keys of the steps that ran.
async fn migrate_up_to_finalise(rig: &Rig, wanted: &Catalog) -> Vec<String> {
    let p = rig.plan(wanted).await;
    assert!(p.refused.is_empty(), "{:?}", p.refused);
    let n = p.steps.len();
    assert!(n >= 2, "{:?}", p.steps);
    assert!(matches!(p.steps[n - 1].run, Run::Finalise));
    let options = ApplyOptions {
        stop_after_step: Some(n - 2),
        ..ApplyOptions::default()
    };
    let err = apply(&rig.backend, &p, options).await.unwrap_err();
    assert!(
        matches!(err, MigrateError::Interrupted { step } if step == n - 2),
        "{err:?}"
    );
    p.steps[..n - 1].iter().map(|s| s.key.clone()).collect()
}

async fn table_count(rig: &Rig) -> i64 {
    rig.db
        .raw_client()
        .await
        .query_one(
            "SELECT count(*) FROM pg_class WHERE relnamespace = to_regnamespace(current_schema())::oid \
             AND relkind = 'r'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

async fn create_extension(rig: &Rig, name: &str) {
    rig.db
        .raw_client()
        .await
        .batch_execute(&format!("CREATE EXTENSION IF NOT EXISTS {name}"))
        .await
        .unwrap();
}

async fn insert(rig: &Rig, unit: &str, from: Option<&str>, to: &str) -> Result<u64, String> {
    let range = from.map(|f| format!("tstzrange('{f}', '{to}')"));
    rig.db
        .raw_client()
        .await
        .execute(
            &format!(
                "INSERT INTO booking (id, version, unit, during) \
                 VALUES (gen_random_uuid(), 1, $1, {})",
                range.as_deref().unwrap_or("NULL")
            ),
            &[&unit],
        )
        .await
        .map_err(|e| e.code().map_or("-----".to_owned(), |c| c.code().to_owned()))
}

#[tokio::test]
async fn a_missing_extension_refuses_and_applies_nothing_then_the_install_lets_it_through() {
    let Some(rig) = rig().await else { return };
    let wanted = bookings(1, &["unit"], vec![]);

    // The plan built with what the database has refuses and says how to fix it.
    let dry = dry_run(&rig.backend, &wanted, &PlanOptions::default())
        .await
        .unwrap();
    assert_eq!(dry.refused.len(), 1, "{:?}", dry.refused);
    assert_eq!(dry.refused[0].code, codes::EXTENSION_UNAVAILABLE);
    assert!(dry.refused[0].reason.contains("extension.install"));
    assert!(dry.refused[0].reason.contains("btree_gist"));
    assert!(matches!(
        apply(&rig.backend, &dry, ApplyOptions::default()).await,
        Err(MigrateError::Refused(_))
    ));

    // A plan built without the inventory is refused by `apply` itself, before any step.
    let blind = rig.plan(&wanted).await;
    assert!(blind.refused.is_empty());
    let err = apply(&rig.backend, &blind, ApplyOptions::default())
        .await
        .unwrap_err();
    let MigrateError::Refused(refusals) = &err else {
        panic!("expected a refusal, got {err:?}");
    };
    assert_eq!(refusals[0].code, codes::EXTENSION_UNAVAILABLE);
    assert_eq!(
        table_count(&rig).await,
        0,
        "no DDL ran after the refusal, not even the bookkeeping table"
    );

    // The audited install is out of scope here; the test creates the extension itself.
    create_extension(&rig, "btree_gist").await;
    let executed = migrate_up_to_finalise(&rig, &wanted).await;
    assert!(
        executed
            .iter()
            .any(|k| k == "add_exclusion:booking.unit_during"),
        "{executed:?}"
    );
    let again = rig.plan(&wanted).await;
    assert!(again.is_empty(), "{:?} {:?}", again.steps, again.refused);
    let dry = dry_run(&rig.backend, &wanted, &PlanOptions::default())
        .await
        .unwrap();
    assert!(dry.is_empty(), "{:?} {:?}", dry.steps, dry.refused);

    // The constraint is named with the prefix, and it holds.
    let names: Vec<String> = rig
        .db
        .raw_client()
        .await
        .query(
            "SELECT conname::text FROM pg_constraint WHERE contype = 'x'",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(names, ["spot_booking_unit_during_excl"]);
    assert_eq!(
        insert(&rig, "a", Some("2026-01-01"), "2026-01-03").await,
        Ok(1)
    );
    // Adjacent: `[3,5)` after `[1,3)` commits (half-open).
    assert_eq!(
        insert(&rig, "a", Some("2026-01-03"), "2026-01-05").await,
        Ok(1)
    );
    // Overlapping the same unit fails with the exclusion violation.
    assert_eq!(
        insert(&rig, "a", Some("2026-01-02"), "2026-01-04").await,
        Err("23P01".to_owned())
    );
    // Another unit, and a null range, never conflict.
    assert_eq!(
        insert(&rig, "b", Some("2026-01-02"), "2026-01-04").await,
        Ok(1)
    );
    assert_eq!(insert(&rig, "a", None, "2026-01-04").await, Ok(1));
    assert_eq!(insert(&rig, "a", None, "2026-01-04").await, Ok(1));
}

#[tokio::test]
async fn an_old_extension_refuses_with_the_version_code_and_changes_nothing() {
    let Some(rig) = rig().await else { return };
    create_extension(&rig, "btree_gist").await;
    let wanted = bookings(1, &["unit"], vec![]);
    let mut options = PlanOptions::default();
    options
        .extension_min_versions
        .insert("btree_gist".into(), "99.0".into());

    let dry = dry_run(&rig.backend, &wanted, &options).await.unwrap();
    assert_eq!(dry.refused.len(), 1, "{:?}", dry.refused);
    assert_eq!(dry.refused[0].code, codes::EXTENSION_VERSION);
    assert!(dry.refused[0].reason.contains("extension.install"));

    let blind = rig.plan_with_defaults(&wanted, &options).await;
    let err = apply(&rig.backend, &blind, ApplyOptions::default())
        .await
        .unwrap_err();
    let MigrateError::Refused(refusals) = &err else {
        panic!("expected a refusal, got {err:?}");
    };
    assert_eq!(refusals[0].code, codes::EXTENSION_VERSION);
    assert_eq!(table_count(&rig).await, 0);
}

#[tokio::test]
async fn a_requirement_step_in_a_plan_is_checked_and_never_installs() {
    let Some(rig) = rig().await else { return };
    let wanted = bookings(1, &[], vec![]);
    let mut p = rig.plan(&wanted).await;
    p.steps.insert(0, extension_step("pg_trgm", "1.0"));
    assert!(matches!(p.steps[0].run, Run::Extension { .. }));
    let err = apply(&rig.backend, &p, ApplyOptions::default())
        .await
        .unwrap_err();
    let MigrateError::Refused(refusals) = &err else {
        panic!("expected a refusal, got {err:?}");
    };
    assert_eq!(refusals[0].code, codes::EXTENSION_UNAVAILABLE);
    assert_eq!(table_count(&rig).await, 0);
    let installed: i64 = rig
        .db
        .raw_client()
        .await
        .query_one(
            "SELECT count(*) FROM pg_extension WHERE extname = 'pg_trgm'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(installed, 0, "a migration never creates an extension");
    // Once it is there, the same requirement passes.
    create_extension(&rig, "pg_trgm").await;
    let mut p = rig.plan(&wanted).await;
    let n = p.steps.len();
    p.steps.insert(0, extension_step("pg_trgm", "1.0"));
    let options = ApplyOptions {
        stop_after_step: Some(n - 2),
        ..ApplyOptions::default()
    };
    let err = apply(&rig.backend, &p, options).await.unwrap_err();
    assert!(matches!(err, MigrateError::Interrupted { .. }), "{err:?}");
}

#[tokio::test]
async fn a_single_column_exclusion_needs_no_extension() {
    let Some(rig) = rig().await else { return };
    let wanted = bookings(1, &[], vec![gist_on_during()]);
    let executed = migrate_up_to_finalise(&rig, &wanted).await;
    assert!(executed.iter().any(|k| k.starts_with("add_exclusion:")));
    assert!(rig.plan(&wanted).await.is_empty());
    // The GiST index carries the prefix and the `_gix` suffix.
    let indexes: Vec<String> = rig
        .db
        .raw_client()
        .await
        .query(
            "SELECT indexname::text FROM pg_indexes WHERE tablename = 'booking' \
             AND indexdef LIKE '%USING gist%' ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        indexes,
        ["spot_booking_during_gix", "spot_booking_unit_during_excl"]
    );
    // One range, one booking at a time, across the whole table.
    assert_eq!(
        insert(&rig, "a", Some("2026-02-01"), "2026-02-03").await,
        Ok(1)
    );
    assert_eq!(
        insert(&rig, "b", Some("2026-02-02"), "2026-02-04").await,
        Err("23P01".to_owned())
    );
}

#[tokio::test]
async fn an_exclusion_added_to_a_populated_table_validates_the_rows_first() {
    let Some(rig) = rig().await else { return };
    create_extension(&rig, "btree_gist").await;
    migrate_up_to_finalise(&rig, &bookings_plain(1)).await;
    insert(&rig, "a", Some("2026-03-01"), "2026-03-03")
        .await
        .unwrap();
    insert(&rig, "a", Some("2026-03-03"), "2026-03-05")
        .await
        .unwrap();

    let wanted = bookings(2, &["unit"], vec![]);
    let p = rig.plan(&wanted).await;
    let step = p
        .steps
        .iter()
        .find(|s| s.key == "add_exclusion:booking.unit_during")
        .unwrap();
    assert_eq!(step.risk, Risk::NeedsValidation);
    assert!(matches!(step.run, Run::Outside(_)));
    let executed = migrate_up_to_finalise(&rig, &wanted).await;
    assert!(executed.iter().any(|k| k.starts_with("add_exclusion:")));
    assert!(rig.plan(&wanted).await.is_empty());
    assert_eq!(
        insert(&rig, "a", Some("2026-03-02"), "2026-03-04").await,
        Err("23P01".to_owned())
    );
}

#[tokio::test]
async fn existing_overlaps_fail_the_build_whole_and_leak_no_row_values() {
    let Some(rig) = rig().await else { return };
    create_extension(&rig, "btree_gist").await;
    migrate_up_to_finalise(&rig, &bookings_plain(1)).await;
    insert(&rig, "SECRET-UNIT-7", Some("2026-04-01"), "2026-04-04")
        .await
        .unwrap();
    insert(&rig, "SECRET-UNIT-7", Some("2026-04-02"), "2026-04-05")
        .await
        .unwrap();

    let wanted = bookings(2, &["unit"], vec![]);
    let p = rig.plan(&wanted).await;
    let err = apply(&rig.backend, &p, ApplyOptions::default())
        .await
        .unwrap_err();
    let MigrateError::Database { message } = &err else {
        panic!("expected the database error, got {err:?}");
    };
    assert!(message.starts_with("23P01"), "{message}");
    assert!(
        message.contains("spot_booking_unit_during_excl"),
        "{message}"
    );
    assert!(!message.contains("SECRET"), "{message}");
    // Nothing of the failed build is left: no constraint, no index, the rows are intact.
    let left: i64 = rig
        .db
        .raw_client()
        .await
        .query_one(
            "SELECT (SELECT count(*) FROM pg_constraint WHERE conname = 'spot_booking_unit_during_excl') \
                  + (SELECT count(*) FROM pg_indexes WHERE indexname = 'spot_booking_unit_during_excl')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 0);
    let rows: i64 = rig
        .db
        .raw_client()
        .await
        .query_one("SELECT count(*) FROM booking", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 2);
    // After the data is repaired the same plan goes through.
    rig.db
        .raw_client()
        .await
        .batch_execute("DELETE FROM booking WHERE upper(during) = '2026-04-05'::timestamptz")
        .await
        .unwrap();
    migrate_up_to_finalise(&rig, &wanted).await;
    assert!(rig.plan(&wanted).await.is_empty());
}

#[tokio::test]
async fn a_trigram_index_needs_pg_trgm_and_is_named_and_stable_once_it_has_it() {
    let Some(rig) = rig().await else { return };
    let trgm = IndexDef {
        name: n("unit_trgm"),
        fields: vec![n("unit")],
        unique: false,
        method: IndexMethod::Gin,
        opclass: Some(OpClass::GinTrgm),
        include: vec![],
    };
    let wanted = bookings(1, &[], vec![trgm]);
    let dry = dry_run(&rig.backend, &wanted, &PlanOptions::default())
        .await
        .unwrap();
    assert_eq!(dry.refused.len(), 1, "{:?}", dry.refused);
    assert_eq!(dry.refused[0].code, codes::EXTENSION_UNAVAILABLE);
    assert!(dry.refused[0].reason.contains("pg_trgm"));
    create_extension(&rig, "pg_trgm").await;
    migrate_up_to_finalise(&rig, &wanted).await;
    let again = dry_run(&rig.backend, &wanted, &PlanOptions::default())
        .await
        .unwrap();
    assert!(again.is_empty(), "{:?} {:?}", again.steps, again.refused);
    let name: String = rig
        .db
        .raw_client()
        .await
        .query_one(
            "SELECT indexname::text FROM pg_indexes WHERE indexdef LIKE '%gin_trgm_ops%'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(name, "spot_booking_unit_trgm_ginx");
}
