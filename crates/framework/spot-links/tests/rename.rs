//! Rename: links survive because they are ids, and where-used is identical before and after.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Harness, text};
use spot_app_api::prelude::Value;

#[tokio::test]
async fn a_rename_leaves_every_reference_valid_and_where_used_unchanged() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Acme"))]).await;
    let invoice = h
        .create(
            "invoice",
            &[
                ("number", text("INV-1")),
                ("customer", Value::Id(customer.id())),
            ],
        )
        .await;

    let before = h.where_used("customer", customer.id()).await.expect("runs");

    let renamed = h
        .rename("customer", &customer, "Acme Holdings")
        .await
        .expect("the rename succeeds");
    assert_eq!(renamed.id(), customer.id(), "the id never changes");

    // The reference is still the same id, so the invoice is still valid.
    let stored_invoice = h
        .stored("invoice", invoice.id())
        .await
        .expect("still stored");
    assert_eq!(
        stored_invoice.get("customer"),
        &Value::Id(customer.id()),
        "the link is an id and survives a rename"
    );
    let issues = h
        .validate("invoice", &stored_invoice)
        .await
        .expect("validation runs");
    assert!(
        issues.is_empty(),
        "the reference is still valid: {issues:?}"
    );

    let after = h.where_used("customer", customer.id()).await.expect("runs");
    assert_eq!(before, after, "where-used is identical across a rename");
}

#[tokio::test]
async fn a_rename_to_a_taken_name_is_refused() {
    let h = Harness::new().await;
    let first = h.create("customer", &[("name", text("Acme"))]).await;
    let _second = h.create("customer", &[("name", text("Globex"))]).await;

    let clash = h
        .check_rename("customer", first.id(), "Globex")
        .await
        .expect_err("the name is taken");
    assert_eq!(clash.code, "links.name_taken");
    assert_eq!(clash.api_code(), "document.duplicate");

    // A fresh name, and the document's own name, are both fine.
    h.check_rename("customer", first.id(), "Acme New")
        .await
        .expect("a fresh name is free");
    h.check_rename("customer", first.id(), "Acme")
        .await
        .expect("a document may keep its own name");
}
