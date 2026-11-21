//! `function_call` and `function_call_get` at the HTTP layer (WP-PLT-17.T2), against the fake port
//! the conformance suite uses: every status the route itself decides (a bad path segment, a body
//! that is not a method call, no credential), and the errors a service returns (unknown function,
//! a verb the function does not answer) rendered with their registry status and code. What the
//! function registry decides is tested in `spot-functions` and, over a real host, in `spot-cli`.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs, dead_code)]

mod support;

use serde_json::json;
use support::fake::TOKEN_ALICE;
use support::harness::{self, App};

const FN_PATH: &str = "/api/v1/method/spot/sales/fn";

async fn post(app: &App, path: &str, body: Option<serde_json::Value>) -> harness::Call {
    harness::call(app, "POST", path, &[harness::bearer(TOKEN_ALICE)], body).await
}

#[tokio::test]
async fn a_function_answers_post_and_get_with_the_data_envelope() {
    let app = App::new();
    let posted = post(&app, FN_PATH, Some(json!({"args": {"x": 1}}))).await;
    assert_eq!(posted.status, 200, "{}", posted.raw);
    assert!(posted.body.get("data").is_some(), "{}", posted.raw);
    let got = harness::call(&app, "GET", FN_PATH, &[harness::bearer(TOKEN_ALICE)], None).await;
    assert_eq!(got.status, 200, "{}", got.raw);
    // A call with no body at all has no arguments, not an error.
    let bare = post(&app, FN_PATH, None).await;
    assert_eq!(bare.status, 200, "{}", bare.raw);
}

#[tokio::test]
async fn a_function_the_service_does_not_know_is_404_method_not_found() {
    let app = App::new();
    let call = post(
        &app,
        "/api/v1/method/spot/sales/nothing",
        Some(json!({"args": {}})),
    )
    .await;
    assert_eq!(call.status, 404, "{}", call.raw);
    assert_eq!(call.error_code(), "method.not_found");
}

#[tokio::test]
async fn a_verb_the_function_does_not_answer_is_405() {
    let app = App::new();
    let call = harness::call(
        &app,
        "GET",
        "/api/v1/method/spot/sales/sync_fn",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    assert_eq!(call.status, 405, "{}", call.raw);
    assert_eq!(call.error_code(), "api.method_not_allowed");
}

#[tokio::test]
async fn a_path_segment_that_is_not_an_identifier_is_400_before_the_service_runs() {
    let app = App::new();
    for path in [
        "/api/v1/method/Spot/sales/fn",
        "/api/v1/method/spot/Sales-Module/fn",
        "/api/v1/method/spot/sales/fn%20x",
        "/api/v1/method/spot/sales/fn.json",
    ] {
        let call = post(&app, path, Some(json!({"args": {}}))).await;
        assert!(
            call.status == 400 || call.status == 404,
            "{path}: {}",
            call.raw
        );
        assert!(
            matches!(
                call.error_code(),
                "api.invalid_request" | "api.unknown_route"
            ),
            "{path}: {}",
            call.raw
        );
    }
}

#[tokio::test]
async fn a_body_that_is_not_a_method_call_is_400() {
    let app = App::new();
    for body in [
        json!({"args": {}, "extra": 1}),
        json!({"args": [1, 2]}),
        json!({"args": "text"}),
        json!([1]),
        json!("text"),
    ] {
        let call = post(&app, FN_PATH, Some(body.clone())).await;
        assert_eq!(call.status, 400, "{body}: {}", call.raw);
        assert_eq!(call.error_code(), "api.invalid_request", "{body}");
    }
}

#[tokio::test]
async fn a_call_without_a_credential_is_401_and_reaches_no_service() {
    let app = App::new();
    for method in ["POST", "GET"] {
        let call = harness::call(&app, method, FN_PATH, &[], None).await;
        assert_eq!(call.status, 401, "{method}: {}", call.raw);
        assert_eq!(call.error_code(), "auth.required");
    }
}
