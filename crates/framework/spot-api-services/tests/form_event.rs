//! The form-event adapter (WP-UI-01.T1): the diff, the permissions it hides behind, and the
//! bounds. The golden suite in `golden.rs` replays `tests/golden/`; this file proves the rules
//! around it — write-free, deterministic, permission-filtered, bounded and cancellable.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod form_support;

use std::time::Duration;

use form_support::{
    CUSTOMER_ACME, DOC, Forms, ITEM_GADGET, ITEM_WIDGET, NOBODY, ROW_ONE, ROW_SCOPED, ROW_TWO, ctx,
};
use serde_json::{Value as Json, json};
use spot_api_services::form_event::{FormEvent, FormEventService, Limits, RateLimit};
use spot_http::api::codec::parse_record_id;
use spot_storage_contract::{ChangeFeed, FeedPosition};

fn request() -> Json {
    json!({
        "doc": {"number": "INV-1", "lines": [{
            "id": ROW_ONE, "idx": 1, "item": ITEM_WIDGET, "qty": "2", "rate": "12.50"
        }]},
        "changed": "lines[1].qty"
    })
}

fn code(error: &spot_http::api::service::ApiError) -> &str {
    error.code.as_str()
}

async fn outbox_len(forms: &Forms) -> usize {
    forms
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 100)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn a_line_change_answers_only_the_derived_fields() {
    let forms = Forms::new().await;
    let event = forms.call(&request()).await.unwrap();
    let patch = event.patch.as_object().unwrap();
    let mut keys: Vec<&str> = patch.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["grand_total", "lines", "subtotal"]);
    assert_eq!(patch["subtotal"], "25.00");
    assert_eq!(patch["grand_total"], "25.00");
    let row = &patch["lines"][0];
    assert_eq!(row["id"], ROW_ONE);
    assert_eq!(row["idx"], 1);
    assert_eq!(row["amount"], "25.00");
    // The change the client made is not echoed back, and no draft value is repeated.
    assert!(row.get("qty").is_none());
    assert!(row.get("rate").is_none());
    assert!(event.messages.is_empty());
}

#[tokio::test]
async fn a_hidden_field_never_appears_and_its_messages_are_dropped() {
    let forms = Forms::new().await;
    let body = json!({
        "doc": {"number": "INV-21", "hidden_note": "client text",
                "lines": [{"id": ROW_ONE, "item": ITEM_WIDGET, "qty": "1", "rate": "1.00"}]},
        "changed": "number"
    });
    let event = forms.call(&body).await.unwrap();
    let text = event.to_json().to_string();
    assert!(!text.contains("hidden_note"), "{text}");
    assert!(!text.contains("audit:INV-21"), "{text}");
    assert!(event.patch.get("hidden_note").is_none());
    // The message about the hidden field is gone; the one about the readable masked field stays.
    assert_eq!(event.messages.len(), 1, "{:?}", event.messages);
    assert_eq!(event.messages[0].field.as_deref(), Some("secret"));
}

#[tokio::test]
async fn a_masked_field_appears_only_in_its_masked_form() {
    let forms = Forms::new().await;
    let body = json!({"doc": {"number": "INV-21"}, "changed": "number"});
    let event = forms.call(&body).await.unwrap();
    let masked = event.patch["secret"].as_str().unwrap();
    assert_eq!(masked, "******V-21");
    let bytes = event.to_json().to_string();
    assert!(
        !bytes.contains("SEC-INV-21"),
        "the real value leaked: {bytes}"
    );
}

#[tokio::test]
async fn a_stale_base_version_is_not_an_error_for_a_pure_derivation() {
    let forms = Forms::new().await;
    let mut stale = request();
    stale["doc"]["id"] = Json::String(form_support::DOC.to_owned());
    stale["base_version"] = Json::from(1);
    let mut very_stale = stale.clone();
    very_stale["base_version"] = Json::from(999);

    let one = forms.call(&stale).await.unwrap();
    let two = forms.call(&very_stale).await.unwrap();
    assert_eq!(one, two);
    assert_eq!(one.patch["grand_total"], "25.00");
    // A base version below 1 does not fit the contract's schema.
    let mut zero = stale.clone();
    zero["base_version"] = Json::from(0);
    assert_eq!(
        code(&forms.call(&zero).await.unwrap_err()),
        "api.invalid_request"
    );
}

#[tokio::test]
async fn the_event_writes_nothing_and_enqueues_nothing() {
    let forms = Forms::new().await;
    let before = forms.commits().await;
    let event = forms.call(&request()).await.unwrap();
    assert_eq!(event.patch["grand_total"], "25.00");
    assert_eq!(
        forms.commits().await,
        before,
        "a form event committed a write"
    );
    assert_eq!(
        outbox_len(&forms).await,
        0,
        "a form event wrote an outbox row"
    );
}

#[tokio::test]
async fn a_repeat_answers_the_same_bytes_even_for_a_row_without_an_id() {
    let forms = Forms::new().await;
    let body = json!({
        "doc": {"number": "INV-6", "lines": [
            {"id": ROW_ONE, "item": ITEM_WIDGET, "qty": "1", "rate": "12.50"},
            {"item": ITEM_WIDGET, "qty": "2", "rate": "3.00"}
        ]},
        "changed": "lines"
    });
    let one = forms.call(&body).await.unwrap();
    let two = forms.call(&body).await.unwrap();
    assert_eq!(
        serde_json::to_vec(&one.to_json()).unwrap(),
        serde_json::to_vec(&two.to_json()).unwrap()
    );
    let id = one.patch["lines"][1]["id"].as_str().unwrap();
    assert!(
        parse_record_id(id).is_some(),
        "the assigned id is not a UUIDv7: {id}"
    );
}

#[tokio::test]
async fn the_doctype_and_field_paths_are_validated() {
    let forms = Forms::new().await;
    let body = request();
    async fn call(
        forms: &Forms,
        doctype: &str,
        body: &Json,
    ) -> Result<FormEvent, spot_http::api::service::ApiError> {
        let bytes = body.to_string();
        forms
            .service
            .form_event(&ctx(), doctype, bytes.as_bytes(), Duration::from_secs(5))
            .await
    }
    assert_eq!(
        code(&call(&forms, "nope", &body).await.unwrap_err()),
        "meta.unknown_doctype"
    );
    let mut unknown = body.clone();
    unknown["changed"] = Json::String("nope".to_owned());
    assert_eq!(
        code(&call(&forms, "invoice", &unknown).await.unwrap_err()),
        "document.unknown_field"
    );
    let mut unknown_child = body.clone();
    unknown_child["changed"] = Json::String("lines[1].nope".to_owned());
    assert_eq!(
        code(&call(&forms, "invoice", &unknown_child).await.unwrap_err()),
        "document.unknown_field"
    );
    let mut no_step = body.clone();
    no_step["changed"] = Json::String("subtotal".to_owned());
    assert_eq!(
        code(&call(&forms, "invoice", &no_step).await.unwrap_err()),
        "form.field_unknown"
    );
    let mut row_missing = body.clone();
    row_missing["changed"] = Json::String("lines[9].qty".to_owned());
    assert_eq!(
        code(&call(&forms, "invoice", &row_missing).await.unwrap_err()),
        "api.invalid_request"
    );
    let mut malformed = body.clone();
    malformed["changed"] = Json::String("lines[0].qty".to_owned());
    assert_eq!(
        code(&call(&forms, "invoice", &malformed).await.unwrap_err()),
        "api.invalid_request"
    );
}

#[tokio::test]
async fn an_unreadable_changed_path_is_indistinguishable_from_an_unknown_one() {
    let forms = Forms::new().await;
    let mut results = Vec::new();
    for path in ["hidden_note", "no_such_field", "lines[1].no_such_col"] {
        let mut body = request();
        body["changed"] = Json::String(path.to_owned());
        let error = forms.call(&body).await.unwrap_err();
        assert_eq!(code(&error), "document.unknown_field", "{path}");
        assert!(error.details.is_empty(), "{path}");
        results.push((path, error));
    }
    // Same code, same message shape and same field issue, differing only in the path the
    // caller itself supplied.
    for (path, error) in &results {
        assert_eq!(error.fields.len(), 1, "{path}");
        assert_eq!(error.fields[0].field, *path);
        assert_eq!(error.fields[0].code, "document.unknown_field");
        assert_eq!(
            error.message,
            format!("The DocType has no such field: `{path}`.")
        );
    }
    let hidden = &results[0].1;
    let unknown = &results[1].1;
    assert_eq!(
        hidden.message.replace("hidden_note", "X"),
        unknown.message.replace("no_such_field", "X")
    );
    assert_eq!(hidden.explanation, unknown.explanation);
}

#[tokio::test]
async fn a_caller_without_the_read_right_is_refused() {
    let forms = Forms::for_user(NOBODY).await;
    let error = forms.call(&request()).await.unwrap_err();
    assert_eq!(code(&error), "permission.denied");
}

#[tokio::test]
async fn a_row_index_out_of_order_is_refused() {
    let forms = Forms::new().await;
    let body = json!({
        "doc": {"number": "INV-X", "lines": [{"id": ROW_ONE, "idx": 2, "qty": "1"}]},
        "changed": "lines[1].qty"
    });
    assert_eq!(
        code(&forms.call(&body).await.unwrap_err()),
        "api.invalid_request"
    );
}

#[tokio::test]
async fn request_bodies_are_validated_as_the_contract_says() {
    let forms = Forms::new().await;
    assert_eq!(
        code(&forms.raw(b"not json").await.unwrap_err()),
        "api.invalid_json"
    );
    assert_eq!(
        code(&forms.raw(b"[]").await.unwrap_err()),
        "api.invalid_request"
    );
    let mut extra = request();
    extra["version"] = Json::from(1);
    assert_eq!(
        code(&forms.call(&extra).await.unwrap_err()),
        "api.invalid_request"
    );
    let mut no_changed = request();
    no_changed.as_object_mut().unwrap().remove("changed");
    assert_eq!(
        code(&forms.call(&no_changed).await.unwrap_err()),
        "api.invalid_request"
    );
    let mut state = request();
    state["doc"]["_state"] = Json::String("draft".to_owned());
    assert_eq!(
        code(&forms.call(&state).await.unwrap_err()),
        "api.invalid_request"
    );
}

#[tokio::test]
async fn the_body_and_child_row_caps_are_enforced() {
    let forms = Forms::new().await;
    let tiny = forms.service_with(Limits {
        max_body_bytes: 32,
        ..Limits::default()
    });
    let error = tiny
        .form_event(
            &ctx(),
            "invoice",
            request().to_string().as_bytes(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&error), "http.body_too_large");

    let one_row = forms.service_with(Limits {
        max_child_rows: 1,
        ..Limits::default()
    });
    let body = json!({
        "doc": {"number": "INV-X", "lines": [
            {"item": ITEM_WIDGET, "qty": "1", "rate": "1.00"},
            {"item": ITEM_WIDGET, "qty": "2", "rate": "1.00"}
        ]},
        "changed": "lines"
    });
    let error = one_row
        .form_event(
            &ctx(),
            "invoice",
            body.to_string().as_bytes(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&error), "api.invalid_request");
    assert!(error.message.contains("child rows"), "{}", error.message);
}

#[tokio::test]
async fn the_rate_limit_is_per_user_and_answers_retry_after() {
    let forms = Forms::new().await;
    let limited = forms.service_with(Limits {
        rate: RateLimit {
            burst: 1,
            window: Duration::from_secs(60),
        },
        ..Limits::default()
    });
    let first = limited
        .form_event(
            &ctx(),
            "invoice",
            request().to_string().as_bytes(),
            Duration::from_secs(5),
        )
        .await;
    assert!(first.is_ok());
    let second = limited
        .form_event(
            &ctx(),
            "invoice",
            request().to_string().as_bytes(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&second), "api.rate_limited");
    assert!(second.details.contains_key("retry_after"), "{second:?}");
}

#[tokio::test]
async fn a_step_that_outlives_the_deadline_answers_http_timeout() {
    let forms = Forms::system_clock().await;
    let body = json!({"doc": {"number": "INV-SLOW"}, "changed": "customer"});
    let error = forms
        .service
        .form_event(
            &ctx(),
            "invoice",
            body.to_string().as_bytes(),
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&error), "http.timeout");
}

#[tokio::test]
async fn dropping_the_future_leaves_nothing_behind() {
    let forms = Forms::system_clock().await;
    let before = forms.commits().await;
    let body = json!({"doc": {"number": "INV-SLOW"}, "changed": "customer"});
    let bytes = body.to_string();
    let dropped = tokio::time::timeout(Duration::from_millis(20), async {
        forms
            .service
            .form_event(&ctx(), "invoice", bytes.as_bytes(), Duration::from_secs(5))
            .await
    })
    .await;
    assert!(dropped.is_err(), "the slow step should not have finished");
    assert_eq!(forms.commits().await, before);
    assert_eq!(outbox_len(&forms).await, 0);
}

#[tokio::test]
async fn a_draft_with_a_user_chosen_id_and_no_rows_is_fine() {
    let forms = Forms::new().await;
    let body = json!({
        "doc": {"id": form_support::DOC, "number": "INV-31", "customer": CUSTOMER_ACME},
        "changed": "customer"
    });
    let event = forms.call(&body).await.unwrap();
    assert_eq!(event.patch["customer_name"], "Acme Ltd");
    let _ = DOC;
}

#[tokio::test]
async fn the_service_is_one_implementation_for_every_protocol() {
    // The body arrives from a browser (compact, keys in one order) and from the desktop client
    // (pretty-printed, keys in another); the decoded answer must be the same object.
    let forms = Forms::new().await;
    let browser = br#"{"doc":{"number":"INV-P","lines":[{"id":"01970000-0000-7000-8000-0000000000f1","item":"01970000-0000-7000-8000-0000000000e1","qty":"2","rate":"12.50"}]},"changed":"lines[1].qty"}"#;
    let desktop = serde_json::to_string_pretty(&json!({
        "changed": "lines[1].qty",
        "doc": {"number": "INV-P", "lines": [{
            "rate": "12.50", "qty": "2",
            "item": "01970000-0000-7000-8000-0000000000e1",
            "id": "01970000-0000-7000-8000-0000000000f1"
        }]}
    }))
    .unwrap();
    let one = forms.raw(browser).await.unwrap();
    let two = forms.raw(desktop.as_bytes()).await.unwrap();
    assert_eq!(
        serde_json::to_vec(&one.to_json()).unwrap(),
        serde_json::to_vec(&two.to_json()).unwrap()
    );
}

fn error_request(number: &str) -> Json {
    json!({"doc": {"number": number}, "changed": "number"})
}

#[tokio::test]
async fn a_step_error_naming_a_hidden_field_is_reduced_to_its_code() {
    let forms = Forms::new().await;
    for number in ["INV-ERR-HIDDEN", "INV-ERR-TEXT"] {
        let error = forms.call(&error_request(number)).await.unwrap_err();
        let text = format!("{error:?}");
        assert!(!text.contains("hidden_note"), "{number}: {text}");
        assert!(error.fields.is_empty(), "{number}: {text}");
        assert!(error.details.is_empty(), "{number}: {text}");
        assert!(error.explanation.is_none(), "{number}: {text}");
        assert_eq!(code(&error), "document.validation_failed", "{number}");
    }
}

#[tokio::test]
async fn a_step_error_about_a_readable_field_keeps_its_issues() {
    let forms = Forms::new().await;
    let error = forms
        .call(&error_request("INV-ERR-OPEN"))
        .await
        .unwrap_err();
    assert_eq!(code(&error), "document.validation_failed");
    assert_eq!(error.message, "The customer is closed.");
    assert_eq!(error.fields.len(), 1);
    assert_eq!(error.fields[0].field, "customer");
    assert!(error.details.contains_key("closed"));
}

#[tokio::test]
async fn a_user_scoped_on_a_child_link_gets_no_columns_of_a_row_outside_the_scope() {
    let forms = Forms::for_user(ROW_SCOPED).await;
    let body = json!({
        "doc": {"number": "INV-S", "lines": [
            {"id": ROW_ONE, "idx": 1, "item": ITEM_WIDGET, "qty": "2", "rate": "1.00"},
            {"id": ROW_TWO, "idx": 2, "item": ITEM_GADGET, "qty": "3", "rate": "1.00"}
        ]},
        "changed": "lines[1].item"
    });
    let event = forms.call(&body).await.unwrap();
    let lines = event.patch["lines"].as_array().unwrap();
    let inside = lines[0].as_object().unwrap();
    assert_eq!(inside["item_name"], "Widget", "{inside:?}");
    let outside = lines[1].as_object().unwrap();
    let mut keys: Vec<&str> = outside.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["id", "idx"], "{outside:?}");
    let text = event.to_json().to_string();
    assert!(!text.contains("Gadget"), "{text}");
    assert!(!text.contains("7.25"), "{text}");
}

#[tokio::test]
async fn a_changed_row_outside_the_users_scope_is_refused_as_unknown() {
    let forms = Forms::for_user(ROW_SCOPED).await;
    let body = json!({
        "doc": {"number": "INV-S", "lines": [
            {"id": ROW_ONE, "idx": 1, "item": ITEM_WIDGET, "qty": "2", "rate": "1.00"},
            {"id": ROW_TWO, "idx": 2, "item": ITEM_GADGET, "qty": "3", "rate": "1.00"}
        ]},
        "changed": "lines[2].qty"
    });
    let error = forms.call(&body).await.unwrap_err();
    assert_eq!(code(&error), "document.unknown_field");
}
