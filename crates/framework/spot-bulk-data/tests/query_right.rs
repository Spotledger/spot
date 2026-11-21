//! Security (WP-SEC-04.T13): a filter or a sort on a protected field needs the `report`
//! right in the export channel, not `read`. A principal who may read a field but not report
//! it learns nothing about its ordering or values from an export query.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{DOCTYPE, TestAuthority, build, field_set, principal};
use spot_bulk_data::{EXPORT_ALIAS, ExportPlan, ExportSpec, OrderKey};
use spot_storage_contract::{CmpOp, Expr, Name, Value};

fn filter_on(field: &str, value: &str) -> Expr {
    Expr::Cmp(
        Box::new(Expr::field(
            &Name::new(EXPORT_ALIAS).unwrap(),
            &Name::new(field).unwrap(),
        )),
        CmpOp::Eq,
        Box::new(Expr::Value(Value::Text(value.into()))),
    )
}

fn build_plan(
    world: &common::World,
    spec: &ExportSpec,
    user: &str,
) -> Result<ExportPlan, spot_bulk_data::BulkError> {
    ExportPlan::build(
        spec,
        &principal(user),
        &world.guard,
        &TestAuthority,
        &field_set(),
        &*world.protection,
    )
}

#[tokio::test]
async fn read_without_report_refuses_a_filter_or_a_sort_on_a_protected_field() {
    let world = build(6).await;
    let columns = ["id", "title"];
    let filtered = ExportSpec::csv(DOCTYPE)
        .with_columns(&columns)
        .with_filter(filter_on("card_number", "4111-1111-1111-0000"));
    let sorted = ExportSpec::csv(DOCTYPE)
        .with_columns(&columns)
        .with_order(vec![OrderKey::asc("salary")]);
    for (what, spec, path) in [
        ("filter", &filtered, "card_number"),
        ("sort", &sorted, "salary"),
    ] {
        let err = build_plan(&world, spec, "readnr")
            .err()
            .unwrap_or_else(|| panic!("{what} must be refused"));
        assert_eq!(err.code(), "permission.field_denied", "{what}");
        assert!(err.message().contains(path), "{what} names the path");
        assert!(
            !err.message().contains("4111") && !err.message().contains("doc "),
            "{what}: no value in the refusal: {}",
            err.message()
        );
    }
}

#[tokio::test]
async fn the_same_queries_pass_with_both_rights() {
    let world = build(6).await;
    let columns = ["id", "title"];
    let filtered = ExportSpec::csv(DOCTYPE)
        .with_columns(&columns)
        .with_filter(filter_on("card_number", "4111-1111-1111-0000"));
    let sorted = ExportSpec::csv(DOCTYPE)
        .with_columns(&columns)
        .with_order(vec![OrderKey::asc("salary")]);
    for spec in [&filtered, &sorted] {
        build_plan(&world, spec, "analyst").expect("both rights: allowed");
    }
}

#[tokio::test]
async fn a_masked_reporter_is_still_refused() {
    // Report right, but only the masked form: ordering would leak the real value.
    let world = build(6).await;
    let sorted = ExportSpec::csv(DOCTYPE)
        .with_columns(&["id", "title"])
        .with_order(vec![OrderKey::asc("salary")]);
    let err = build_plan(&world, &sorted, "masked")
        .map(|_| ())
        .expect_err("refused");
    assert_eq!(err.code(), "permission.field_denied");
}
