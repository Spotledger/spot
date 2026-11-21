//! The PATCH behaviour review (ADR-055: merge semantics approved as *preliminary*).
//!
//! One named test per scenario the architecture owner asked for. Each asserts what happens
//! **now**, and carries a comment saying whether a client would find the result surprising.
//! The suites change nothing in the contract; the findings go to the report.

#[allow(dead_code)]
mod support;

use serde_json::Value as Json;
use support::Must;
use support::fake::TOKEN_ALICE;
use support::fake::TOKEN_BOB;
use support::harness::{self, App};

/// A canonical UUIDv7 chosen as a row id (ADR-055: clients may choose ids).
fn chosen_id(seed: u8) -> String {
    format!("0192f3c4-5b6a-7c8d-9e0f-0000000000{seed:02x}")
}

/// Seeds a customer, an item and a draft invoice; returns `(invoice, customer, item)` ids.
async fn seed_invoice(app: &App) -> (String, String, String) {
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(app, alice, "PATCH customer").await;
    customer.expect_status(201, "seed customer");
    let item = harness::create_doc(
        app,
        alice,
        "item",
        serde_json::json!({ "name": "Patch item" }),
    )
    .await;
    item.expect_status(201, "seed item");
    let invoice = harness::create_doc(
        app,
        alice,
        "sales_invoice",
        serde_json::json!({
            "name": "SINV-P",
            "customer": customer.doc_id(),
        }),
    )
    .await;
    invoice.expect_status(201, "seed invoice");
    (invoice.doc_id(), customer.doc_id(), item.doc_id())
}

/// The invoice's `items` rows as the API returns them.
async fn rows_of(app: &App, invoice: &str) -> Vec<Json> {
    let probe = harness::get_doc(app, TOKEN_ALICE, "sales_invoice", invoice).await;
    probe.expect_status(200, "load invoice");
    probe.body["data"]["items"].as_array().must().clone()
}

// --- Scenario 1 -------------------------------------------------------------
// Sending one changed row of a 10-row child table (the other 9 left out) deletes the 9.
#[tokio::test]
async fn one_row_sent_deletes_the_other_nine() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, alice, "ACME Hyderabad").await;
    customer.expect_status(201, "seed customer");
    let item =
        harness::create_doc(&app, alice, "item", serde_json::json!({ "name": "Screw" })).await;
    item.expect_status(201, "seed item");
    let mut body = serde_json::json!({ "name": "SINV-10", "customer": customer.doc_id() });
    body["items"] = Json::Array(
        (0..10)
            .map(|index| {
                serde_json::json!({ "item": item.doc_id(), "qty": "1.00", "rate": format!("{index}.00") })
            })
            .collect(),
    );
    let invoice = harness::create_doc(&app, alice, "sales_invoice", body).await;
    invoice.expect_status(201, "create 10 rows");
    let invoice_id = invoice.doc_id();
    let rows_before = rows_of(&app, &invoice_id).await;
    assert_eq!(rows_before.len(), 10);
    let kept = rows_before[4]["id"].as_str().must().to_owned();

    // The patch says what the client believes the table should be: one kept row, changed.
    let probe = harness::patch_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice_id,
        &[harness::if_match(invoice.doc_version())],
        serde_json::json!({ "items": [
            { "id": kept, "item": item.doc_id(), "qty": "9.99", "rate": "4.00" }
        ] }),
    )
    .await;
    probe.expect_status(200, "the row-replacement patch is accepted");
    let rows = rows_of(&app, &invoice_id).await;
    assert_eq!(rows.len(), 1, "the nine rows not sent are deleted");
    assert_eq!(rows[0]["id"].as_str().must(), kept);
    assert_eq!(rows[0]["qty"].as_str().must(), "9.99");
    // Client surprise: HIGH. A client that loads one row and patches it erases the rest of
    // the table unless it sends every row back. Finding: an explicit `rows_patch` form (or a
    // table-level replace flag) would make row deletion an act the client states instead of
    // a side effect of omission.
}

// --- Scenario 2 -------------------------------------------------------------
// A row without `id` next to rows with ids is inserted; `idx` follows the order sent.
#[tokio::test]
async fn a_row_without_id_is_inserted_at_the_position_sent() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let (invoice, _customer, item) = seed_invoice(&app).await;
    let kept_id = chosen_id(0xA0);
    let probe = harness::patch_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice,
        &[harness::if_match(1)],
        serde_json::json!({ "items": [
            { "id": kept_id, "item": item, "qty": "1.00", "rate": "1.00" },
            { "item": item, "qty": "2.00", "rate": "2.00" }
        ] }),
    )
    .await;
    probe.expect_status(200, "patch with a row without id");
    let rows = rows_of(&app, &invoice).await;
    assert_eq!(rows.len(), 2, "the two sent rows are the whole table");
    assert_eq!(
        rows[0]["id"].as_str().must(),
        kept_id,
        "the chosen id is kept"
    );
    let assigned = rows[1]["id"].as_str().must();
    assert!(
        !assigned.is_empty() && *assigned != kept_id,
        "the new row's id is server-chosen: {assigned}"
    );
    assert_eq!(rows[0]["idx"], 1, "idx follows the order sent");
    assert_eq!(rows[1]["idx"], 2);
    // Client surprise: MEDIUM. Insertion by omission is what a merge patch implies, and the
    // response names every id (chosen and server-assigned) and every idx, so a client that
    // re-reads or echoes the response stays sound.
}

// --- Scenario 3 -------------------------------------------------------------
// A row with an id this document does not have is inserted with that id; a foreign id is a
// typed duplicate error; the same id twice in one table is `document.invalid_row`.
#[tokio::test]
async fn foreign_duplicated_chosen_row_ids() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let (invoice_a, customer, item) = seed_invoice(&app).await;
    // Invoice B owns a row id A must not claim.
    let row_b = harness::create_doc(
        &app,
        alice,
        "sales_invoice",
        serde_json::json!({
            "name": "SINV-B",
            "customer": customer,
            "items": [ { "id": chosen_id(0xB0), "item": item, "qty": "1.00", "rate": "1.00" } ]
        }),
    )
    .await;
    row_b.expect_status(201, "invoice B with a chosen row id");

    // (a) An id this document does not have, and nobody owns: inserted with that id.
    let fresh = chosen_id(0xB1);
    let probe = harness::patch_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice_a,
        &[harness::if_match(1)],
        serde_json::json!({ "items": [ { "id": fresh, "item": item, "qty": "3.00", "rate": "3.00" } ] }),
    )
    .await;
    probe.expect_status(200, "a fresh chosen id inserts");
    let rows = rows_of(&app, &invoice_a).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"].as_str().must(), fresh);

    // (b) An id another document's row owns: `document.duplicate` (409), a typed error.
    let probe = harness::patch_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice_a,
        &[harness::if_match(2)],
        serde_json::json!({ "items": [ { "id": chosen_id(0xB0), "item": item, "qty": "3.00", "rate": "3.00" } ] }),
    )
    .await;
    probe.expect_status(409, "a foreign row id");
    assert_eq!(probe.error_code(), "document.duplicate");

    // (c) The same id twice in one table: `document.invalid_row` (422, in fields).
    let twice = chosen_id(0xB2);
    let probe = harness::patch_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice_a,
        &[harness::if_match(2)],
        serde_json::json!({ "items": [
            { "id": twice, "item": item, "qty": "3.00", "rate": "3.00" },
            { "id": twice, "item": item, "qty": "3.00", "rate": "3.00" }
        ] }),
    )
    .await;
    probe.expect_status(422, "the same row id twice");
    assert_eq!(
        probe.error()["fields"][0]["code"].as_str().must(),
        "document.invalid_row",
        "the field error names the document.invalid_row code"
    );
    // Client surprise: LOW. All three outcomes are typed and distinct; choosing ids is the
    // ADR-055 offline/POS use, and conflicts are surfaced instead of silently merged.
}

// --- Scenario 4 -------------------------------------------------------------
// `null` for a field sets null; leaving the field out keeps it; `""` stays distinct from null.
#[tokio::test]
async fn null_sets_absent_keeps_and_empty_string_is_not_null() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, alice, "ACME Sargodha").await;
    customer.expect_status(201, "seed customer");
    let id = customer.doc_id();

    // Set two values, then null one out.
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(customer.doc_version())],
        serde_json::json!({ "category": "retail", "phone": "0300-111" }),
    )
    .await;
    probe.expect_status(200, "two fields set");
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(2)],
        serde_json::json!({ "category": null }),
    )
    .await;
    probe.expect_status(200, "null sets null");
    assert!(
        probe.body["data"]["category"].is_null(),
        "the field reads null"
    );
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(3)],
        serde_json::json!({ "phone": "0300-222" }),
    )
    .await;
    probe.expect_status(200, "phone set again, category left out");
    assert!(
        probe.body["data"]["category"].is_null(),
        "leaving the field out keeps it (still null)"
    );
    // "" against null: distinct through the whole round trip.
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(4)],
        serde_json::json!({ "category": "" }),
    )
    .await;
    probe.expect_status(200, "empty string sets");
    assert_eq!(
        probe.body["data"]["category"].as_str().must(),
        "",
        "\"\" is a string, never null"
    );
    // Client surprise: LOW — this is the documented, tested one-to-one rule (ADR-055).
}

// --- Scenario 5 -------------------------------------------------------------
// Two clients patch different fields from the same version: the second gets 412, even though
// the changes do not overlap.
#[tokio::test]
async fn non_overlapping_patches_from_the_same_version_still_conflict() {
    let app = App::new();
    let customer = harness::seed_customer(&app, TOKEN_ALICE, "ACME Gujrat").await;
    customer.expect_status(201, "seed customer");
    let id = customer.doc_id();
    let first = harness::patch_doc(
        &app,
        TOKEN_ALICE,
        "customer",
        &id,
        &[harness::if_match(customer.doc_version())],
        serde_json::json!({ "category": "retail" }),
    )
    .await;
    first.expect_status(200, "the first client wins");
    let second = harness::patch_doc(
        &app,
        TOKEN_BOB,
        "customer",
        &id,
        &[harness::if_match(customer.doc_version())],
        serde_json::json!({ "phone": "0300-333" }),
    )
    .await;
    second.expect_status(412, "the second client's base is stale");
    assert_eq!(second.error_code(), "document.stale_version");
    assert_eq!(
        second.error()["details"]["current_version"].as_u64().must(),
        2
    );
    // Client surprise: MEDIUM-LOW. Textbook optimistic concurrency: the version, not the
    // field, is the lock. A client re-reads and patches again. Note for the review: without
    // field-level merge, busy documents with many independent editors retry visibly; the
    // realtime feed (WP-PLT-08) is the designed mitigation.
}

// --- Scenario 6 -------------------------------------------------------------
// A patch that changes nothing (the same values): does the version increase? Recorded here.
#[tokio::test]
async fn a_no_op_patch_still_bumps_the_version() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, alice, "ACME Sialkot").await;
    customer.expect_status(201, "seed customer");
    let id = customer.doc_id();
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(customer.doc_version())],
        serde_json::json!({ "category": "retail" }),
    )
    .await;
    probe.expect_status(200, "set category");
    let etag_before = probe.etag();

    // The same value again: a no-op patch.
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(2)],
        serde_json::json!({ "category": "retail" }),
    )
    .await;
    probe.expect_status(200, "the no-op patch is accepted");
    // Recorded behaviour NOW: the version increments (and the ETag moves), even though the
    // stored values did not change.
    assert_eq!(
        probe.doc_version(),
        3,
        "a no-op patch still increments the version"
    );
    assert_eq!(probe.etag(), "\"3\"");
    assert_ne!(probe.etag(), etag_before, "the ETag moves with the version");
    // Client surprise: MEDIUM. A client that patches a value it just read gets a new
    // If-Match base each time, so two clients echoing the same write ping-pong versions.
    // Finding: consider detecting no-op writes in the engine (changed_fields is empty) and
    // answering 200 with the unchanged document, version untouched.
}

// --- Scenario 7 -------------------------------------------------------------
// A patch on a posted document touching one allowed and one read-only field: the whole
// request is rejected, nothing is applied.
#[tokio::test]
async fn a_mixed_patch_on_a_posted_document_is_rejected_whole() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let (invoice, _customer, _item) = seed_invoice(&app).await;
    let posted = harness::post_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice,
        &[harness::if_match(1)],
    )
    .await;
    posted.expect_status(200, "post the invoice");

    // `notes` is allowed after posting; `total` is not. The whole request must be rejected.
    let probe = harness::patch_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice,
        &[harness::if_match(posted.doc_version())],
        serde_json::json!({ "notes": "ok after posting", "total": "999.00" }),
    )
    .await;
    probe.expect_status(422, "the mixed patch is rejected");
    assert_eq!(probe.error_code(), "document.validation_failed");
    let paths = probe.field_paths();
    assert!(
        paths.iter().any(|field| field == "total"),
        "the read-only field is named: {paths:?}"
    );
    // Recorded behaviour NOW: the request is all-or-nothing (the allowed `notes` is NOT
    // applied), and the error lists only the offending fields (here `total` alone; the
    // allowed `notes` is not listed).
    let after = harness::get_doc(&app, alice, "sales_invoice", &invoice).await;
    assert!(
        after.body["data"]["notes"].is_null(),
        "nothing was applied from the rejected request"
    );
    // Client surprise: MEDIUM. All-or-nothing is the safe choice, but the error names only
    // the offending fields, so a client cannot tell whether its other field was fine for
    // reasons of state or merely unexamined. Finding: list every touched field with its
    // verdict (`allowed`/`read_only`), as the contract's `fields` array can carry per-field
    // codes already.
}

// --- Scenario 8 -------------------------------------------------------------
// A patch to an engine-derived field (`amount` from qty × rate) sent together with a change
// to `qty`: the fake's DeriveAmount controller recomputes `amount` on every save, so this
// shows what a client sees when its derived-field write races the engine's own value.
#[tokio::test]
async fn a_derived_field_patch_is_recomputed_by_the_engine() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let (invoice, _customer, item) = seed_invoice(&app).await;
    let probe = harness::patch_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice,
        &[harness::if_match(1)],
        serde_json::json!({ "items": [
            { "item": item, "qty": "2.00", "rate": "3.00" }
        ] }),
    )
    .await;
    probe.expect_status(200, "seed one row");
    let amount_before = rows_of(&app, &invoice).await.remove(0)["amount"]
        .as_str()
        .must()
        .to_owned();
    assert_eq!(amount_before, "6.00", "the engine derived 2.00 × 3.00");

    // One request carries both: a client change to qty AND an explicit amount.
    let probe = harness::patch_doc(
        &app,
        alice,
        "sales_invoice",
        &invoice,
        &[harness::if_match(2)],
        serde_json::json!({ "items": [
            { "item": item, "qty": "5.00", "rate": "3.00", "amount": "99.00" }
        ] }),
    )
    .await;
    probe.expect_status(200, "the patch is accepted");
    let rows = rows_of(&app, &invoice).await;
    assert_eq!(
        rows[0]["amount"].as_str().must(),
        "15.00",
        "5.00 × 3.00 — the engine's value"
    );
    assert_eq!(rows[0]["qty"].as_str().must(), "5.00");
    // Recorded behaviour NOW: the engine recomputes `amount` in its before-save step, so the
    // client's explicit `amount: "99.00"` is discarded without an error; the engine attaches
    // an info message (`selling.amount_derived`) naming the field, and the response document
    // shows the engine's value.
    let messages = probe.body["messages"].as_array().must();
    assert!(
        messages
            .iter()
            .any(|m| m["code"].as_str().must() == "selling.amount_derived"),
        "the engine says what it recomputed: {messages:?}"
    );
    // Client surprise: MEDIUM. A client sending a derived field is silently overruled; the
    // message names the recomputation and the response shows the effective value, but a
    // client that does not read `messages` sees its number vanish. Finding: engine-derived
    // fields should be marked read-only in the meta (the contract's `read_only` flag exists
    // for exactly this), so the write is rejected up front instead of recomputed.
}
