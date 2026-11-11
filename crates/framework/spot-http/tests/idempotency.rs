//! The `Idempotency-Key` protocol over the wire (ADR-055): replays, conflicts, TTL expiry on
//! a `ManualClock`, two concurrent same-key requests, and the release rules (a 415/400 never
//! holds the key; a 5xx releases it).

#[allow(dead_code)]
mod support;

use serde_json::Value as Json;
use std::time::Duration;
use support::Must;
use support::fake::TOKEN_ALICE;
use support::harness::{self, App};

/// One 24-hour TTL plus a minute: past any live entry.
const TTL_AND_A_BIT: Duration = Duration::from_secs(24 * 60 * 60 + 60);

fn customer_body(name: &str) -> Json {
    serde_json::json!({ "name": name })
}

#[tokio::test]
async fn a_repeat_replays_the_stored_response() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let path = "/api/v1/resource/customer";
    let head = [harness::bearer(alice), harness::idem_key("inv-1")];
    let first = harness::call(
        &app,
        "POST",
        path,
        &head,
        Some(customer_body("ACME replay")),
    )
    .await;
    first.expect_status(201, "the first create runs");
    assert!(
        first.header("idempotency-replayed").is_none(),
        "the first response is live"
    );
    let second = harness::call(
        &app,
        "POST",
        path,
        &head,
        Some(customer_body("ACME replay")),
    )
    .await;
    second.expect_status(201, "the replay answers the stored status");
    assert_eq!(
        second.header("idempotency-replayed").must(),
        "true",
        "the replay is stamped"
    );
    assert_eq!(
        second.raw, first.raw,
        "the replayed body is the stored body, byte for byte"
    );
    // Same key, different body: a conflict, whatever the fingerprint order.
    let probe = harness::call(&app, "POST", path, &head, Some(customer_body("ACME other"))).await;
    probe.expect_status(409, "same key, different body");
    assert_eq!(probe.error_code(), "api.idempotency_conflict");
}

#[tokio::test]
async fn ttl_expiry_frees_the_key() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let path = "/api/v1/resource/customer";
    let head = [harness::bearer(alice), harness::idem_key("ttl-1")];
    let first = harness::call(&app, "POST", path, &head, Some(customer_body("ACME ttl"))).await;
    first.expect_status(201, "the first create runs");
    app.clock.advance(TTL_AND_A_BIT);
    let second = harness::call(&app, "POST", path, &head, Some(customer_body("ACME ttl"))).await;
    second.expect_status(201, "after the TTL the key is fresh");
    assert!(
        second.header("idempotency-replayed").is_none(),
        "an expired entry is not a replay; the command runs again"
    );
    assert_ne!(
        second.body["data"]["id"].as_str().must(),
        first.body["data"]["id"].as_str().must(),
        "a fresh run creates a new document"
    );
    assert_eq!(app.fake.stored_count_of("customer"), 2);
}

#[tokio::test]
async fn two_concurrent_same_key_requests_run_one_command() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let path = "/api/v1/resource/customer";
    let head = [harness::bearer(alice), harness::idem_key("race-1")];
    let (first, second) = tokio::join!(
        harness::call(&app, "POST", path, &head, Some(customer_body("ACME race"))),
        harness::call(&app, "POST", path, &head, Some(customer_body("ACME race"))),
    );
    first.expect_status(201, "the winner's create");
    second.expect_status(201, "the loser's replay");
    assert_eq!(
        first.body["data"]["id"].as_str().must(),
        second.body["data"]["id"].as_str().must(),
        "both requests describe the same document"
    );
    let replayed = [
        first.header("idempotency-replayed"),
        second.header("idempotency-replayed"),
    ];
    assert_eq!(
        replayed.iter().filter(|seen| seen.is_some()).count(),
        1,
        "exactly one response is the replay: {replayed:?}"
    );
    assert_eq!(
        app.fake.stored_count_of("customer"),
        1,
        "one command ran; the second request replays"
    );
}

#[tokio::test]
async fn a_415_or_400_never_holds_the_key() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let path = "/api/v1/resource/customer";

    // 415 first: the key is not reserved.
    let probe = harness::call(
        &app,
        "POST",
        path,
        &[
            harness::bearer(alice),
            harness::idem_key("hold-1"),
            ("content-type", "text/plain".to_owned()),
        ],
        Some(customer_body("ACME x")),
    )
    .await;
    probe.expect_status(415, "wrong media type");
    let ok = harness::call(
        &app,
        "POST",
        path,
        &[harness::bearer(alice), harness::idem_key("hold-1")],
        Some(customer_body("ACME x")),
    )
    .await;
    ok.expect_status(201, "the 415 never held the key: this is a fresh run");
    assert!(ok.header("idempotency-replayed").is_none());

    // 400 invalid JSON: also before the reservation.
    let probe = harness::call(
        &app,
        "POST",
        path,
        &[harness::bearer(alice), harness::idem_key("hold-2")],
        None,
    )
    .await;
    probe.expect_status(400, "empty body");
    let ok = harness::call(
        &app,
        "POST",
        path,
        &[harness::bearer(alice), harness::idem_key("hold-2")],
        Some(customer_body("ACME y")),
    )
    .await;
    ok.expect_status(201, "the 400 never held the key");
    assert!(ok.header("idempotency-replayed").is_none());
}

#[tokio::test]
async fn a_5xx_releases_the_key_so_a_retry_runs_fresh() {
    let app = App::new();
    let alice = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, alice, "ACME Faisalabad").await;
    customer.expect_status(201, "seed customer");
    let invoice = harness::create_doc(
        &app,
        alice,
        "sales_invoice",
        serde_json::json!({ "name": "SINV-R", "customer": customer.doc_id() }),
    )
    .await;
    invoice.expect_status(201, "seed invoice");

    app.fake.arm_internal_totals_failure();
    let method_path = format!(
        "/api/v1/resource/sales_invoice/{}/methods/totals",
        invoice.doc_id()
    );
    let head = [
        harness::bearer(alice),
        harness::if_match(invoice.doc_version()),
        harness::idem_key("boom-1"),
    ];
    let failed = harness::call(&app, "POST", &method_path, &head, None).await;
    failed.expect_status(500, "the armed failure");
    let retried = harness::call(&app, "POST", &method_path, &head, None).await;
    retried.expect_status(200, "the retry runs fresh after the 5xx");
    assert!(
        retried.header("idempotency-replayed").is_none(),
        "a released key never replays the failed response"
    );
}
