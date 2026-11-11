//! The where-used query: grouping, counts, the bounded sample and child-row reporting.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Harness, name, row, text};
use spot_app_api::prelude::{RecordId, Value};
use spot_links::WhereUsed;

fn group<'a>(
    used: &'a WhereUsed,
    doctype: &str,
    field: &str,
) -> Option<&'a spot_links::ReferrerGroup> {
    used.groups
        .iter()
        .find(|g| g.doctype == doctype && g.field == field)
}

#[tokio::test]
async fn where_used_groups_by_doctype_and_field() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Acme"))]).await;

    // invoice.customer
    h.create(
        "invoice",
        &[
            ("number", text("INV-1")),
            ("customer", Value::Id(customer.id())),
        ],
    )
    .await;

    // invoice line's billing_customer: reported under the parent invoice.
    let mut invoice = spot_app_api::Document::new(name("invoice"), RecordId::new_v7());
    invoice.set(name("number"), text("INV-2"));
    invoice.rows_mut(name("lines")).push(row(&[
        ("item", text("Widget")),
        ("billing_customer", Value::Id(customer.id())),
    ]));
    h.create_doc(invoice).await;

    // payment.customer
    h.create(
        "payment",
        &[
            ("reference", text("PAY-1")),
            ("customer", Value::Id(customer.id())),
        ],
    )
    .await;

    // note.about (dynamic link)
    h.create(
        "note",
        &[
            ("about_type", text("customer")),
            ("about", Value::Id(customer.id())),
        ],
    )
    .await;

    let used = h.where_used("customer", customer.id()).await.expect("runs");
    assert_eq!(used.total, 4, "four references: {used:?}");
    assert_eq!(
        group(&used, "invoice", "customer").map(|g| g.count),
        Some(1)
    );
    assert_eq!(
        group(&used, "invoice", "billing_customer").map(|g| g.count),
        Some(1),
        "a child row reports its parent document"
    );
    assert_eq!(
        group(&used, "payment", "customer").map(|g| g.count),
        Some(1)
    );
    assert_eq!(group(&used, "note", "about").map(|g| g.count), Some(1));
}

#[tokio::test]
async fn where_used_reports_a_multiselect_under_its_parent() {
    let h = Harness::new().await;
    let tag = h.create("tag", &[("name", text("vip"))]).await;
    let mut customer = spot_app_api::Document::new(name("customer"), RecordId::new_v7());
    customer.set(name("name"), text("Acme"));
    customer
        .rows_mut(name("tags"))
        .push(row(&[("tag", Value::Id(tag.id()))]));
    let customer = h.create_doc(customer).await;

    let used = h.where_used("tag", tag.id()).await.expect("runs");
    let g = group(&used, "customer", "tag").expect("the multiselect row refers");
    assert_eq!(g.count, 1, "{used:?}");
    assert_eq!(g.sample, vec![customer.id()], "the parent id is reported");
}

#[tokio::test]
async fn where_used_of_an_unreferenced_document_is_empty() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Lonely"))]).await;
    let used = h.where_used("customer", customer.id()).await.expect("runs");
    assert_eq!(used.total, 0);
    assert!(used.groups.is_empty());
}

#[tokio::test]
async fn where_used_of_an_unknown_doctype_errors() {
    let h = Harness::new().await;
    let error = h
        .where_used("widget", RecordId::new_v7())
        .await
        .expect_err("unknown DocType");
    assert_eq!(error.code, "meta.unknown_doctype");
}

#[tokio::test]
async fn where_used_counts_a_thousand_and_bounds_the_sample() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Big"))]).await;
    for i in 0..1000 {
        h.create(
            "payment",
            &[
                ("reference", Value::Text(format!("PAY-{i}"))),
                ("customer", Value::Id(customer.id())),
            ],
        )
        .await;
    }
    let used = h.where_used("customer", customer.id()).await.expect("runs");
    assert_eq!(used.total, 1000);
    let g = group(&used, "payment", "customer").expect("the payments refer");
    assert_eq!(g.count, 1000, "the count is exact");
    assert_eq!(
        g.sample.len(),
        spot_links::DEFAULT_SAMPLE,
        "the sample is bounded to the default"
    );

    let bounded = h
        .where_used_limit("customer", customer.id(), 5)
        .await
        .expect("runs");
    let g = group(&bounded, "payment", "customer").expect("the payments refer");
    assert_eq!(g.count, 1000);
    assert_eq!(g.sample.len(), 5, "the sample honours a custom limit");
}
