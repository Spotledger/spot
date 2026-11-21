//! Concurrency: `If-Match`/`ETag` (ADR-055 "Optimistic concurrency"). Every route that
//! changes a document answers `428 api.precondition_required` without `If-Match` and
//! `412 document.stale_version` when the version moved; reads, creates and writes carry
//! `ETag`, `meta_get` answers `304` with `If-None-Match`, creates carry `Location`.

#[allow(dead_code)]
mod support;

use support::Must;
use support::fake::TOKEN_ALICE;
use support::fake::TOKEN_BOB;
use support::harness::{self, App};

/// Seeds one customer, one item and one draft invoice; returns their ids.
async fn seed(app: &App) -> (String, String, String) {
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(app, alice, "ACME Multan").await;
    customer.expect_status(201, "seed customer");
    let item = harness::create_doc(app, alice, "item", serde_json::json!({ "name": "Nut" })).await;
    item.expect_status(201, "seed item");
    let invoice = harness::create_doc(
        app,
        alice,
        "sales_invoice",
        serde_json::json!({
            "name": "SINV-C",
            "customer": customer.doc_id(),
            "items": [ { "qty": "2.00", "rate": "3.00", "item": item.doc_id() } ]
        }),
    )
    .await;
    invoice.expect_status(201, "seed invoice");
    (customer.doc_id(), item.doc_id(), invoice.doc_id())
}

#[tokio::test]
async fn every_write_route_requires_if_match() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let (customer, item, invoice) = seed(&app).await;
    let version = harness::get_doc(&app, alice, "customer", &customer)
        .await
        .doc_version();

    let patch = harness::patch_doc(
        &app,
        alice,
        "customer",
        &customer,
        &[],
        serde_json::json!({ "category": "d" }),
    )
    .await;
    patch.expect_status(428, "PATCH without If-Match");
    assert_eq!(patch.error_code(), "api.precondition_required");
    let probe = harness::delete_doc(&app, alice, "customer", &customer, &[]).await;
    probe.expect_status(428, "DELETE without If-Match");
    let probe = harness::post_doc(&app, alice, "sales_invoice", &invoice, &[]).await;
    probe.expect_status(428, "post without If-Match");
    let probe = harness::cancel_doc(&app, alice, "sales_invoice", &invoice, &[]).await;
    probe.expect_status(428, "cancel without If-Match");
    let probe = harness::call(
        &app,
        "POST",
        &format!("/api/v1/resource/customer/{customer}/rename"),
        &[harness::bearer(alice)],
        Some(serde_json::json!({ "name": "R-1" })),
    )
    .await;
    probe.expect_status(428, "rename without If-Match");
    // A mutating method call: the fake's `totals` is declared mutating.
    let probe = harness::call(
        &app,
        "POST",
        &format!("/api/v1/resource/sales_invoice/{invoice}/methods/totals"),
        &[harness::bearer(alice)],
        None,
    )
    .await;
    probe.expect_status(428, "mutating method without If-Match");
    // A GET needs nothing.
    let probe = harness::get_doc(&app, alice, "customer", &customer).await;
    probe.expect_status(200, "GET is unconstrained");
    let _ = (item, version);
}

#[tokio::test]
async fn correct_requires_if_match_before_anything_else() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let (customer, _item, _invoice) = seed(&app).await;
    // On an unposted DocType `correct` fails with a state error *with* If-Match; without it,
    // the 428 wins, because the precondition precedes the state check.
    let probe = harness::call(
        &app,
        "POST",
        &format!("/api/v1/resource/customer/{customer}/correct"),
        &[harness::bearer(alice)],
        None,
    )
    .await;
    probe.expect_status(428, "correct without If-Match");
}

#[tokio::test]
async fn stale_writes_answer_412_with_the_current_version() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let (customer, _item, invoice) = seed(&app).await;
    let version = harness::get_doc(&app, alice, "customer", &customer)
        .await
        .doc_version();

    // Move the customer forward once.
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &customer,
        &[harness::if_match(version)],
        serde_json::json!({ "category": "retail" }),
    )
    .await;
    probe.expect_status(200, "first patch");

    // A stale base: 412 with details.current_version pointing at the live version.
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &customer,
        &[harness::if_match(version)],
        serde_json::json!({ "category": "wholesale" }),
    )
    .await;
    probe.expect_status(412, "patch on a stale base");
    assert_eq!(probe.error_code(), "document.stale_version");
    assert_eq!(
        probe.error()["details"]["current_version"].as_u64().must(),
        version + 1,
        "details.current_version says what to reload"
    );

    // A stale lifecycle action too.
    let draft_version = harness::get_doc(&app, alice, "sales_invoice", &invoice)
        .await
        .doc_version();
    let probe = harness::post_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice,
        &[harness::if_match(draft_version + 1)],
    )
    .await;
    probe.expect_status(412, "post on a stale base");
    assert_eq!(
        probe.error()["details"]["current_version"].as_u64().must(),
        draft_version
    );
}

#[tokio::test]
async fn etags_track_versions_on_reads_and_writes() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let (customer, _item, invoice) = seed(&app).await;

    // Create: ETag = "1" and version 1, and a Location header.
    let created = harness::seed_customer(&app, alice, "ACME Sukkur").await;
    created.expect_status(201, "create");
    assert_eq!(created.etag(), "\"1\"", "the created document's ETag");
    assert_eq!(
        created.header("location").must(),
        format!("/api/v1/resource/customer/{}", created.doc_id()),
        "Location points at the new document"
    );

    // Read: the same ETag.
    let probe = harness::get_doc(&app, alice, "customer", &created.doc_id()).await;
    probe.expect_status(200, "read");
    assert_eq!(probe.etag(), "\"1\"");

    // Write: a new ETag that matches the new version.
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &created.doc_id(),
        &[harness::if_match(1)],
        serde_json::json!({ "category": "retail" }),
    )
    .await;
    probe.expect_status(200, "patch");
    assert_eq!(probe.etag(), "\"2\"");
    assert_eq!(probe.doc_version(), 2);

    // Another user reading sees the moved version: versions are shared, not per-session.
    let probe = harness::get_doc(&app, TOKEN_BOB, "customer", &created.doc_id()).await;
    assert_eq!(probe.doc_version(), 2);
    let _ = (customer, invoice);
}

#[tokio::test]
async fn meta_get_answers_304_on_if_none_match() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let probe = harness::get_meta(&app, alice, "customer", &[]).await;
    probe.expect_status(200, "meta first read");
    let etag = probe.etag();
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "the meta ETag is a strong tag like \"3\": {etag}"
    );
    let probe =
        harness::get_meta(&app, alice, "customer", &[("if-none-match", etag.clone())]).await;
    probe.expect_status(304, "meta re-read");
    assert_eq!(probe.etag(), etag, "the 304 carries the same ETag");
    let probe = harness::get_meta(
        &app,
        alice,
        "customer",
        &[("if-none-match", "\"999\"".to_owned())],
    )
    .await;
    probe.expect_status(200, "a different tag means a fresh body");
}
