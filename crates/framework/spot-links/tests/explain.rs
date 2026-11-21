//! The typed delete and cancel refusals: `links.referenced` with the grouped referrers.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Harness, name, row, text};
use spot_app_api::prelude::Value;
use spot_storage_contract::{ErrorKind, StorageError};

#[tokio::test]
async fn explain_delete_lists_the_referrers() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Acme"))]).await;
    h.create(
        "invoice",
        &[
            ("number", text("INV-1")),
            ("customer", Value::Id(customer.id())),
        ],
    )
    .await;
    h.create(
        "payment",
        &[
            ("reference", text("PAY-1")),
            ("customer", Value::Id(customer.id())),
        ],
    )
    .await;
    let used = h.where_used("customer", customer.id()).await.expect("runs");
    let def = h.def("customer");
    let error = spot_links::explain_delete(&def, customer.id(), &used);

    assert_eq!(error.code, "links.referenced");
    assert_eq!(error.api_code(), "document.linked");
    assert_eq!(error.details.get("count").and_then(|v| v.as_u64()), Some(2));
    let referrers = error
        .details
        .get("referrers")
        .and_then(|v| v.as_array())
        .expect("the referrers are listed");
    assert_eq!(referrers.len(), 2);
    assert!(
        error.message.contains('2'),
        "the message counts them: {}",
        error.message
    );
    assert!(
        error.explanation.as_deref().is_some_and(|e| !e.is_empty()),
        "the error carries an explanation"
    );
}

#[tokio::test]
async fn explain_cancel_orders_the_ids_newest_first() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Acme"))]).await;
    let first = h
        .create(
            "payment",
            &[
                ("reference", text("PAY-1")),
                ("customer", Value::Id(customer.id())),
            ],
        )
        .await;
    let second = h
        .create(
            "payment",
            &[
                ("reference", text("PAY-2")),
                ("customer", Value::Id(customer.id())),
            ],
        )
        .await;
    let used = h.where_used("customer", customer.id()).await.expect("runs");
    let def = h.def("customer");
    let error = spot_links::explain_cancel(&def, customer.id(), &used);

    let referrers = error
        .details
        .get("referrers")
        .and_then(|v| v.as_array())
        .expect("the referrers are listed");
    let ids: Vec<String> = referrers[0]["ids"]
        .as_array()
        .expect("ids")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], second.id().to_string(), "newest first");
    assert_eq!(ids[1], first.id().to_string());
    assert!(
        error.message.contains("newest first"),
        "the cancel rule is stated: {}",
        error.message
    );
}

#[tokio::test]
async fn explain_cancel_groups_and_counts_a_child_reference() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Acme"))]).await;
    let mut invoice =
        spot_app_api::Document::new(name("invoice"), spot_app_api::prelude::RecordId::new_v7());
    invoice.set(name("number"), text("INV-1"));
    invoice.rows_mut(name("lines")).push(row(&[
        ("item", text("Widget")),
        ("billing_customer", Value::Id(customer.id())),
    ]));
    h.create_doc(invoice).await;
    let used = h.where_used("customer", customer.id()).await.expect("runs");
    let def = h.def("customer");
    let error = spot_links::explain_delete(&def, customer.id(), &used);
    let referrers = error
        .details
        .get("referrers")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("referrers: {error:?}"));
    assert_eq!(referrers[0]["doctype"].as_str(), Some("invoice"));
    assert_eq!(referrers[0]["field"].as_str(), Some("billing_customer"));
    assert_eq!(referrers[0]["count"].as_u64(), Some(1));
}

#[tokio::test]
async fn explain_reference_violation_names_the_field() {
    let h = Harness::new().await;
    let def = h.def("customer");
    let error = StorageError::new(ErrorKind::ReferenceViolation, "still referenced")
        .with_entity(&name("invoice"))
        .with_field(&name("customer"));
    let explained = spot_links::explain_reference_violation(
        &def,
        spot_app_api::prelude::RecordId::new_v7(),
        &error,
    );
    assert_eq!(explained.code, "links.referenced");
    assert_eq!(explained.api_code(), "document.linked");
    assert_eq!(
        explained.details.get("field").and_then(|v| v.as_str()),
        Some("`customer`")
    );
    assert!(
        !explained.message.contains("constraint"),
        "no raw constraint name: {}",
        explained.message
    );
}
