//! The error registry and the error envelope (brief WP-PLT-05.T3, suite "Errors").
//!
//! Two layers:
//! 1. `registry::entries()` must equal the contract's `x-spot-error-codes` mapping, entry for
//!    entry (code, HTTP status, retryable, plain-language meaning);
//! 2. every error response observed over HTTP carries `request_id` and the registry's
//!    `retryable` and HTTP status, and the envelope's `code` is the registry code.

#[allow(dead_code)]
mod support;

use serde_json::Value as Json;
use spot_http::api::registry;
use support::Must;
use support::fake::TOKEN_ALICE;
use support::harness::{self, App};

const CONTRACT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../contracts/native-api/openapi.yaml"
);

/// The contract's registry: code → (status, retryable, meaning), in document order.
fn contract_registry(root: &Json) -> Vec<(String, u16, bool, String)> {
    let table = root["x-spot-error-codes"].as_object().must();
    let mut rows: Vec<(String, u16, bool, String)> = Vec::new();
    for (code, row) in table {
        let items = row.as_array().must();
        assert_eq!(items.len(), 3, "{code}: [status, retryable, meaning]");
        rows.push((
            code.clone(),
            u16::try_from(items[0].as_u64().must()).must(),
            items[1].as_bool().must(),
            items[2].as_str().must().to_owned(),
        ));
    }
    rows
}

/// Asserts an error envelope against the registry row for `code`.
fn assert_envelope(call: &harness::Call, code: &str, what: &str) {
    let entry = registry::entries()
        .into_iter()
        .find(|(c, _, _, _)| *c == code)
        .unwrap_or_else(|| panic!("probe {what}: {code} is not a registry code"));
    call.expect_status(entry.1, what);
    assert_eq!(call.error_code(), code, "{what}: the envelope's code");
    assert_eq!(
        call.error()["retryable"].as_bool().must(),
        entry.2,
        "{what}: the registry's retryable flag"
    );
    let request_id = call.error()["request_id"].as_str().must();
    assert!(
        !request_id.is_empty(),
        "{what}: the error carries a request id"
    );
    assert_eq!(
        call.header("x-request-id").must(),
        request_id,
        "{what}: the echoed header equals the envelope's request id"
    );
}

#[test]
fn registry_table_equals_the_contract() {
    let text = std::fs::read_to_string(CONTRACT).must();
    let root: Json = serde_norway::from_str(&text).must();
    let contract = contract_registry(&root);
    let entries = registry::entries();
    assert_eq!(
        entries.len(),
        contract.len(),
        "the registry and the contract list the same number of codes"
    );
    for (code, status, retryable, meaning) in entries.iter() {
        let row = contract
            .iter()
            .find(|(c, _, _, _)| *c == *code)
            .unwrap_or_else(|| panic!("{code} is missing from the contract"));
        assert_eq!(
            (*status, *retryable, *meaning),
            (row.1, row.2, row.3.as_str()),
            "{code}: the registry row is the contract's"
        );
    }
    // And nothing extra sits in the contract that the registry would not know.
    for (c, _, _, _) in contract.iter() {
        assert!(
            entries.iter().any(|(code, _, _, _)| code == c),
            "{c} is in the contract but not in the registry"
        );
    }
}

/// A canonical valid UUIDv7 that names no document.
const GHOST_ID: &str = "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f";

#[tokio::test]
async fn wire_errors_carry_the_registry_status_retryable_and_request_id() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, alice, "ACME Kārāči").await;
    customer.expect_status(201, "seed customer");
    let id = customer.doc_id();

    // 400 api.invalid_request — `limit` out of range on a list.
    let probe = harness::call(
        &app,
        "GET",
        "/api/v1/resource/customer?limit=0",
        &[harness::bearer(alice)],
        None,
    )
    .await;
    assert_envelope(&probe, "api.invalid_request", "list limit 0");

    // 400 api.invalid_json — a required body that is not JSON.
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &[harness::bearer(alice)],
        None,
    )
    .await;
    assert_envelope(&probe, "api.invalid_json", "empty create body");

    // 415 api.unsupported_media_type.
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &[
            harness::bearer(alice),
            ("content-type", "text/plain".to_owned()),
        ],
        Some(serde_json::json!({ "name": "x" })),
    )
    .await;
    assert_envelope(&probe, "api.unsupported_media_type", "text/plain create");

    // 404 api.unknown_route and 405 api.method_not_allowed (the server core's fallbacks).
    let probe = harness::call(
        &app,
        "GET",
        "/api/v1/definitely/not/a/route",
        &[harness::bearer(alice)],
        None,
    )
    .await;
    assert_envelope(&probe, "api.unknown_route", "unknown path");
    let probe = harness::call(
        &app,
        "GET",
        &format!("/api/v1/resource/customer/{id}/post"),
        &[harness::bearer(alice)],
        None,
    )
    .await;
    assert_envelope(&probe, "api.method_not_allowed", "GET a POST endpoint");

    // 404 http.unknown_site — a host no site is configured for.
    let probe = harness::call_for_host(
        &app,
        "ghost.example.com",
        "GET",
        "/api/v1/resource/customer",
    )
    .await;
    assert_envelope(&probe, "http.unknown_site", "unknown host");

    // 413 http.body_too_large — a body over the server's 64 KiB limit.
    let big = "x".repeat(65 * 1024);
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/resource/customer",
        &[harness::bearer(alice)],
        Some(serde_json::json!({ "name": big })),
    )
    .await;
    assert_envelope(&probe, "http.body_too_large", "oversized body");

    // 401 auth.required — no bearer on a secured route.
    let probe = harness::call(
        &app,
        "GET",
        &format!("/api/v1/resource/customer/{id}"),
        &[],
        None,
    )
    .await;
    assert_envelope(&probe, "auth.required", "no bearer");
}

/// The credential-failure probes: 401/403 codes the auth middleware and fake auth service
/// produce, each asserted against its registry row.
#[tokio::test]
async fn credential_errors_follow_the_registry() {
    let app = App::new();

    // 401 auth.invalid_credentials.
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(serde_json::json!({ "user": "alice@acme.com", "password": "no" })),
    )
    .await;
    assert_envelope(&probe, "auth.invalid_credentials", "wrong password");

    // 401 auth.totp_required, then auth.totp_invalid.
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(serde_json::json!({ "user": support::fake::EVE, "password": "letmein" })),
    )
    .await;
    assert_envelope(
        &probe,
        "auth.totp_required",
        "two-factor user without a code",
    );
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(serde_json::json!({ "user": support::fake::EVE, "password": "letmein", "totp": "111111" })),
    )
    .await;
    assert_envelope(&probe, "auth.totp_invalid", "wrong one-time code");

    // 401 auth.session_expired — a token that was logged out.
    let login = harness::call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(serde_json::json!({ "user": "alice@acme.com", "password": "correct horse" })),
    )
    .await;
    login.expect_status(200, "login fresh");
    let token = login.body["data"]["token"].as_str().must().to_owned();
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/auth/logout",
        &[harness::bearer(&token)],
        None,
    )
    .await;
    probe.expect_status(204, "logout");
    let probe = harness::call(
        &app,
        "GET",
        "/api/v1/auth/session",
        &[harness::bearer(&token)],
        None,
    )
    .await;
    assert_envelope(&probe, "auth.session_expired", "logged-out session");

    // 403 auth.account_disabled.
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(serde_json::json!({ "user": support::fake::DAVE, "password": "hunter2" })),
    )
    .await;
    assert_envelope(&probe, "auth.account_disabled", "disabled user");
}

/// The document-level error codes, each asserted against its registry row.
#[tokio::test]
async fn document_errors_follow_the_registry() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, alice, "ACME Kārāči").await;
    customer.expect_status(201, "seed customer");
    let id = customer.doc_id();
    let version = customer.doc_version();

    // 404 meta.unknown_doctype.
    let probe = harness::get_meta(&app, alice, "nothing", &[]).await;
    assert_envelope(&probe, "meta.unknown_doctype", "unknown doctype meta");

    // 404 document.not_found.
    let probe = harness::get_doc(&app, alice, "customer", GHOST_ID).await;
    assert_envelope(&probe, "document.not_found", "ghost id");

    // 412 document.stale_version, with details.current_version.
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(version + 9)],
        serde_json::json!({ "category": "retail" }),
    )
    .await;
    assert_envelope(&probe, "document.stale_version", "stale If-Match");
    assert_eq!(
        probe.error()["details"]["current_version"].as_u64().must(),
        version,
        "details.current_version says what to reload"
    );

    // 422 document.validation_failed — the required `customer` link missing on an invoice.
    let probe = harness::create_doc(
        &app,
        alice,
        "sales_invoice",
        serde_json::json!({ "name": "SINV-1" }),
    )
    .await;
    assert_envelope(
        &probe,
        "document.validation_failed",
        "required link missing",
    );
    assert_eq!(
        probe.error()["fields"][0]["code"].as_str().must(),
        "document.required",
        "the field error names the code"
    );
    assert_eq!(
        probe.error()["fields"][0]["field"].as_str().must(),
        "customer"
    );

    // 422 document.unknown_field.
    let probe = harness::create_doc(
        &app,
        alice,
        "customer",
        serde_json::json!({ "name": "x", "nme": "typo", "catt": "typo" }),
    )
    .await;
    assert_envelope(&probe, "document.unknown_field", "unknown fields");
    let mut paths = probe.field_paths();
    paths.sort();
    assert_eq!(paths, vec!["catt", "nme"], "every offender listed");

    // 422 document.invalid_value — a JSON number for a decimal field.
    let probe = harness::create_doc(
        &app,
        alice,
        "customer",
        serde_json::json!({ "name": "x", "outstanding": 10.5 }),
    )
    .await;
    assert_envelope(&probe, "document.invalid_value", "number for a decimal");
}

/// The uniqueness, state, link and lookup codes.
#[tokio::test]
async fn conflict_and_lookup_errors_follow_the_registry() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, alice, "ACME Quetta").await;
    customer.expect_status(201, "seed customer");
    let id = customer.doc_id();
    let version = customer.doc_version();
    let item =
        harness::create_doc(&app, alice, "item", serde_json::json!({ "name": "Bolt" })).await;
    item.expect_status(201, "seed item");

    // 409 document.duplicate — the phone uniqueness rule.
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(version)],
        serde_json::json!({ "phone": "+92 300 0000000" }),
    )
    .await;
    probe.expect_status(200, "first phone ok");
    let current_version = probe.doc_version();
    let second = harness::seed_customer(&app, alice, "ACME Lahore").await;
    second.expect_status(201, "second customer");
    let probe = harness::patch_doc(
        &app,
        alice,
        "customer",
        &second.doc_id(),
        &[harness::if_match(second.doc_version())],
        serde_json::json!({ "phone": "+92 300 0000000" }),
    )
    .await;
    assert_envelope(&probe, "document.duplicate", "phone already used");

    // 409 document.invalid_state — cancelling an unposted draft.
    let draft = harness::create_doc(
        &app,
        alice,
        "sales_invoice",
        serde_json::json!({ "name": "SINV-2", "customer": id }),
    )
    .await;
    draft.expect_status(201, "draft invoice");
    let probe = harness::cancel_doc(
        &app,
        alice,
        "sales_invoice",
        &draft.doc_id(),
        &[harness::if_match(draft.doc_version())],
    )
    .await;
    assert_envelope(&probe, "document.invalid_state", "cancel a draft");

    // 409 document.linked — deleting a customer an invoice links to.
    let posted = harness::create_doc(
        &app,
        alice,
        "sales_invoice",
        serde_json::json!({
            "name": "SINV-3",
            "customer": id,
            "items": [ { "qty": "1.00", "rate": "10.00", "item": item.doc_id() } ]
        }),
    )
    .await;
    posted.expect_status(201, "invoice for the customer");
    let probe = harness::delete_doc(
        &app,
        alice,
        "customer",
        &id,
        &[harness::if_match(current_version)],
    )
    .await;
    assert_envelope(&probe, "document.linked", "delete a linked customer");

    // 404 method.not_found and 404 report.not_found.
    let probe = harness::call(
        &app,
        "POST",
        &format!(
            "/api/v1/resource/sales_invoice/{}/methods/nope",
            draft.doc_id()
        ),
        &[
            harness::bearer(alice),
            harness::if_match(draft.doc_version()),
        ],
        None,
    )
    .await;
    assert_envelope(&probe, "method.not_found", "unknown method");
    let probe = harness::call(
        &app,
        "POST",
        "/api/v1/report/nope/run",
        &[harness::bearer(alice)],
        None,
    )
    .await;
    assert_envelope(&probe, "report.not_found", "unknown report");
}

/// 500 http.internal: a service failure renders the registry's row.
#[tokio::test]
async fn internal_errors_follow_the_registry() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, alice, "ACME Islamabad").await;
    customer.expect_status(201, "seed customer");
    let invoice = harness::create_doc(
        &app,
        alice,
        "sales_invoice",
        serde_json::json!({ "name": "SINV-9", "customer": customer.doc_id() }),
    )
    .await;
    invoice.expect_status(201, "seed invoice");
    app.fake.arm_internal_totals_failure();
    let probe = harness::call(
        &app,
        "POST",
        &format!(
            "/api/v1/resource/sales_invoice/{}/methods/totals",
            invoice.doc_id()
        ),
        &[
            harness::bearer(alice),
            harness::if_match(invoice.doc_version()),
        ],
        None,
    )
    .await;
    assert_envelope(&probe, "http.internal", "armed service failure");
}
