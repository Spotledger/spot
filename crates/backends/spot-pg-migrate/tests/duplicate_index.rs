//! A unique index built over existing duplicates: the error names the table, the index and the
//! columns, and never a row value. Runs against a real PostgreSQL (skipped, with a SKIPPED line,
//! when `SPOT_TEST_PG_URL` is unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_pg_migrate::{ApplyOptions, MigrateError, apply, codes};
use spot_storage_contract::{FieldType, StorageBackend};
use support::*;

/// The value both rows hold; the error must never carry it.
const NUMBER: &str = "INV-0001";

fn invoices(index: Option<(&str, &[&str])>) -> spot_storage_contract::Catalog {
    let entity = doc(
        "invoice",
        vec![
            field("number", text(Some(40)), false),
            field("title", text(Some(140)), true),
            field("qty", FieldType::Int, true),
        ],
    );
    let entity = match index {
        Some((name, fields)) => with_index(entity, name, fields, true),
        None => entity,
    };
    catalog(1, vec![entity])
}

/// `true`/`false` for the index's validity, or `None` when the table has no such index.
async fn index_valid(rig: &Rig, name: &str) -> Option<bool> {
    let client = rig.db.raw_client().await;
    client
        .query_opt(
            "SELECT i.indisvalid FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = $1",
            &[&name],
        )
        .await
        .unwrap()
        .map(|row| row.get(0))
}

#[tokio::test]
async fn a_unique_index_over_existing_duplicates_names_the_table_the_index_and_the_columns() {
    let Some(rig) = rig().await else { return };
    // A table with the numbering column, not yet unique.
    rig.migrate(&invoices(None)).await;
    let client = rig.db.raw_client().await;
    client
        .batch_execute(&format!(
            "INSERT INTO invoice (id, version, number, title) VALUES \
             (gen_random_uuid(), 1, '{NUMBER}', 'first'), \
             (gen_random_uuid(), 1, '{NUMBER}', 'second')"
        ))
        .await
        .unwrap();

    // The catalogue now wants the naming backstop.
    let wanted = invoices(Some(("uq_invoice_number", &["number"])));
    let plan = rig.plan(&wanted).await;
    assert!(
        plan.steps
            .iter()
            .any(|s| s.key == "create_index:invoice.uq_invoice_number"),
        "the plan builds the unique index: {:?}",
        plan.steps.iter().map(|s| &s.key).collect::<Vec<_>>()
    );

    let err = apply(&rig.backend, &plan, ApplyOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "migrate.duplicate_values");
    assert_eq!(err.code(), codes::DUPLICATE_VALUES);
    let message = err.to_string();
    for named in ["invoice", "uq_invoice_number", "number"] {
        assert!(
            message.contains(named),
            "{named} is missing from: {message}"
        );
    }
    // Privacy: the server's detail (`Key (number)=(...) is duplicated.`) holds customer data and
    // must never reach the error.
    assert!(
        !message.contains(NUMBER),
        "the error carries the duplicated value: {message}"
    );
    assert!(
        !message.to_lowercase().contains("duplicated"),
        "the error carries the server's detail: {message}"
    );
    assert!(
        !matches!(err, MigrateError::Database { .. }),
        "the plain database error hides the remedy: {err:?}"
    );

    // A failed concurrent build leaves its invalid leftover behind; the next plan drops it and
    // builds the index again, which the planner already does (no row was touched here).
    let leftover = index_valid(&rig, "uq_invoice_number").await;
    assert_eq!(
        leftover,
        Some(false),
        "the failed build left an invalid index"
    );
    let after = rig.plan(&wanted).await;
    assert!(
        after
            .steps
            .iter()
            .any(|s| s.key == "drop_invalid_index:invoice.uq_invoice_number"),
        "the next plan drops the invalid leftover: {:?}",
        after.steps.iter().map(|s| &s.key).collect::<Vec<_>>()
    );

    // Remove one of the two rows, then plan and apply again: the index is built and valid.
    client
        .batch_execute(&format!(
            "DELETE FROM invoice WHERE title = 'second' AND number = '{NUMBER}'"
        ))
        .await
        .unwrap();
    let retry = rig.plan(&wanted).await;
    assert!(retry.refused.is_empty());
    let report = apply(&rig.backend, &retry, ApplyOptions::default())
        .await
        .unwrap();
    assert!(
        report
            .executed
            .iter()
            .any(|k| k == "create_index:invoice.uq_invoice_number"),
        "the rerun builds the index: {:?}",
        report.executed
    );
    assert_eq!(index_valid(&rig, "uq_invoice_number").await, Some(true));
    assert!(rig.plan(&wanted).await.is_empty(), "nothing is left to do");
    rig.backend.prepare(&wanted).await.unwrap();
}
