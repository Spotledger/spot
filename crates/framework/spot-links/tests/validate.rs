//! Link validation: every refusal the brief names, driven through the real engine and the
//! fixture DocTypes.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Harness, name, row, text};
use spot_app_api::prelude::{RecordId, Value};
use spot_links::FieldIssue;

fn code_of(issues: &[FieldIssue], code: &str) -> bool {
    issues.iter().any(|issue| issue.code == code)
}

fn path_of(issues: &[FieldIssue], code: &str) -> Option<String> {
    issues
        .iter()
        .find(|issue| issue.code == code)
        .map(|issue| issue.path.to_string())
}

#[tokio::test]
async fn a_valid_document_has_no_issues() {
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
    let issues = h
        .validate("invoice", &invoice)
        .await
        .expect("validation runs");
    assert!(
        issues.is_empty(),
        "a valid invoice has no issues: {issues:?}"
    );
}

#[tokio::test]
async fn a_missing_target_fails_on_the_field() {
    let h = Harness::new().await;
    // A dangling fixed link never commits (storage's foreign key owns it), so the pre-check is
    // exercised on the unsaved draft the engine would validate before storage is asked.
    let mut invoice = spot_app_api::Document::new(name("invoice"), RecordId::new_v7());
    invoice.set(name("number"), text("INV-2"));
    invoice.set(name("customer"), Value::Id(RecordId::new_v7()));
    let issues = h.validate("invoice", &invoice).await.expect("runs");
    assert!(
        code_of(&issues, spot_links::codes::TARGET_NOT_FOUND),
        "a link to nothing is refused: {issues:?}"
    );
    assert_eq!(
        path_of(&issues, spot_links::codes::TARGET_NOT_FOUND).as_deref(),
        Some("customer")
    );
}

#[tokio::test]
async fn a_child_row_reports_its_row_path() {
    let h = Harness::new().await;
    let mut invoice = spot_app_api::Document::new(name("invoice"), RecordId::new_v7());
    invoice.set(name("number"), text("INV-3"));
    invoice.rows_mut(name("lines")).push(row(&[
        ("item", text("Widget")),
        ("billing_customer", Value::Id(RecordId::new_v7())),
    ]));
    let issues = h.validate("invoice", &invoice).await.expect("runs");
    assert_eq!(
        path_of(&issues, spot_links::codes::TARGET_NOT_FOUND).as_deref(),
        Some("lines[1].billing_customer"),
        "a child row reports `table[idx].field`: {issues:?}"
    );
}

#[tokio::test]
async fn a_dynamic_link_type_that_names_no_doctype_is_refused() {
    let h = Harness::new().await;
    let note = h
        .create_raw(
            "note",
            &[
                ("subject", text("hello")),
                ("about_type", text("widget")),
                ("about", Value::Id(RecordId::new_v7())),
            ],
            spot_app_api::DocState::Draft,
        )
        .await;
    let issues = h.validate("note", &note).await.expect("runs");
    assert!(
        code_of(&issues, spot_links::codes::DYNAMIC_TYPE_UNKNOWN),
        "an unknown dynamic type is refused: {issues:?}"
    );
    assert_eq!(
        path_of(&issues, spot_links::codes::DYNAMIC_TYPE_UNKNOWN).as_deref(),
        Some("about")
    );
}

#[tokio::test]
async fn a_dynamic_link_to_a_child_table_is_refused() {
    let h = Harness::new().await;
    let note = h
        .create_raw(
            "note",
            &[
                ("about_type", text("invoice_line")),
                ("about", Value::Id(RecordId::new_v7())),
            ],
            spot_app_api::DocState::Draft,
        )
        .await;
    let issues = h.validate("note", &note).await.expect("runs");
    assert!(
        code_of(&issues, spot_links::codes::DYNAMIC_TYPE_NOT_ALLOWED),
        "a child-table dynamic type is refused: {issues:?}"
    );
}

#[tokio::test]
async fn a_dynamic_link_of_the_wrong_doctype_is_refused() {
    let h = Harness::new().await;
    let invoice = h.create("invoice", &[("number", text("INV-4"))]).await;
    // The type says customer, but the value is an invoice id: not found among customers.
    let note = h
        .create_raw(
            "note",
            &[
                ("about_type", text("customer")),
                ("about", Value::Id(invoice.id())),
            ],
            spot_app_api::DocState::Draft,
        )
        .await;
    let issues = h.validate("note", &note).await.expect("runs");
    assert_eq!(
        path_of(&issues, spot_links::codes::TARGET_NOT_FOUND).as_deref(),
        Some("about"),
        "a dynamic link to the wrong kind is a bad target: {issues:?}"
    );
}

#[tokio::test]
async fn a_dynamic_link_to_the_right_doctype_passes() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Acme"))]).await;
    let note = h
        .create(
            "note",
            &[
                ("about_type", text("customer")),
                ("about", Value::Id(customer.id())),
            ],
        )
        .await;
    let issues = h.validate("note", &note).await.expect("runs");
    assert!(issues.is_empty(), "a valid dynamic link passes: {issues:?}");
}

#[tokio::test]
async fn a_cancelled_target_is_refused() {
    let h = Harness::new().await;
    // Written around the engine, which no longer lets a link to a cancelled document in.
    let invoice = h
        .create_raw(
            "invoice",
            &[("number", text("INV-5"))],
            spot_app_api::DocState::Cancelled,
        )
        .await;
    let payment = h
        .create_raw(
            "payment",
            &[
                ("reference", text("PAY-1")),
                ("invoice", Value::Id(invoice.id())),
            ],
            spot_app_api::DocState::Draft,
        )
        .await;
    let issues = h.validate("payment", &payment).await.expect("runs");
    assert_eq!(
        path_of(&issues, spot_links::codes::CANCELLED_TARGET).as_deref(),
        Some("invoice"),
        "a cancelled target is refused where forbidden: {issues:?}"
    );
}

#[tokio::test]
async fn a_posted_source_may_not_point_at_a_draft() {
    let h = Harness::new().await;
    let invoice = h.create("invoice", &[("number", text("INV-6"))]).await;
    let payment = h
        .create(
            "payment",
            &[
                ("reference", text("PAY-2")),
                ("invoice", Value::Id(invoice.id())),
            ],
        )
        .await;
    // A draft source may point at a draft.
    let draft_issues = h.validate("payment", &payment).await.expect("runs");
    assert!(
        draft_issues.is_empty(),
        "a draft source is fine: {draft_issues:?}"
    );
    // The posted source may not (written around the engine, which refuses to post it).
    let posted = h
        .create_raw(
            "payment",
            &[
                ("reference", text("PAY-2P")),
                ("invoice", Value::Id(invoice.id())),
            ],
            spot_app_api::DocState::Posted,
        )
        .await;
    let issues = h.validate("payment", &posted).await.expect("runs");
    assert_eq!(
        path_of(&issues, spot_links::codes::DRAFT_TARGET).as_deref(),
        Some("invoice"),
        "a posted document may not point at a draft: {issues:?}"
    );
}

#[tokio::test]
async fn a_table_multiselect_link_is_validated() {
    let h = Harness::new().await;
    let mut customer = spot_app_api::Document::new(name("customer"), RecordId::new_v7());
    customer.set(name("name"), text("Acme"));
    customer
        .rows_mut(name("tags"))
        .push(row(&[("tag", Value::Id(RecordId::new_v7()))]));
    let issues = h.validate("customer", &customer).await.expect("runs");
    assert_eq!(
        path_of(&issues, spot_links::codes::TARGET_NOT_FOUND).as_deref(),
        Some("tags[1].tag"),
        "a multi-select link is validated with its row path: {issues:?}"
    );
}

#[tokio::test]
async fn a_valid_multiselect_passes() {
    let h = Harness::new().await;
    let tag = h.create("tag", &[("name", text("vip"))]).await;
    let mut customer = spot_app_api::Document::new(name("customer"), RecordId::new_v7());
    customer.set(name("name"), text("Acme"));
    customer
        .rows_mut(name("tags"))
        .push(row(&[("tag", Value::Id(tag.id()))]));
    let customer = h.create_doc(customer).await;
    let issues = h.validate("customer", &customer).await.expect("runs");
    assert!(issues.is_empty(), "a valid multi-select passes: {issues:?}");
}

#[tokio::test]
async fn validate_links_error_groups_every_issue() {
    let h = Harness::new().await;
    let mut invoice = spot_app_api::Document::new(name("invoice"), RecordId::new_v7());
    invoice.set(name("number"), text("INV-7"));
    invoice.set(name("customer"), Value::Id(RecordId::new_v7()));
    invoice.rows_mut(name("lines")).push(row(&[
        ("item", text("Widget")),
        ("billing_customer", Value::Id(RecordId::new_v7())),
    ]));
    let issues = h.validate("invoice", &invoice).await.expect("runs");
    let error = spot_links::validate_links_error(issues).expect("issues make an error");
    assert_eq!(error.code, "document.validation_failed");
    assert_eq!(
        error.fields.len(),
        2,
        "both bad links are reported: {error:?}"
    );
}
