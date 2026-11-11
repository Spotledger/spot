//! Contract 0.5.0 suite (brief WP-PLT-05.T5): the permission summary, field reveal, the
//! `_currency` map and the other presentation inputs. Everything runs through the whole
//! router over the fakes in `support/permissions.rs`.

#[allow(dead_code)]
mod support;

use std::sync::{Arc, Mutex};

use proptest::prelude::*;
use serde_json::json;
use spot_http::api::codec::is_field_path;
use spot_http::api::permissions::{DoctypePermissions, FieldSummary, PermissionSummary};
use spot_storage_contract::RecordId;
use support::Must;
use support::fake::TOKEN_ALICE;
use support::harness::{self, App};
use support::permissions::open_entry;

fn bearer() -> [(&'static str, String); 1] {
    [harness::bearer(TOKEN_ALICE)]
}

async fn permissions(app: &App, query: &str, extra: &[(&str, String)]) -> harness::Call {
    let mut headers = bearer().to_vec();
    headers.extend_from_slice(extra);
    harness::call(
        app,
        "GET",
        &format!("/api/v1/auth/permissions{query}"),
        &headers,
        None,
    )
    .await
}

#[tokio::test]
async fn the_summary_answers_with_its_snapshot_etag_and_304() {
    let app = App::new();
    let call = permissions(&app, "?doctype=customer", &[]).await;
    call.expect_status(200, "summary");
    assert_eq!(call.header("etag"), Some("\"42\""));
    assert_eq!(call.body["etag"], "\"42\"");
    let entry = &call.body["data"]["doctypes"][0];
    assert_eq!(entry["doctype"], "customer");
    assert_eq!(entry["rights"]["submit"], true);
    assert_eq!(entry["fields"]["revealable"], json!(["phone"]));

    let same = permissions(
        &app,
        "?doctype=customer",
        &[("if-none-match", "\"42\"".into())],
    )
    .await;
    assert_eq!(same.status, 304);
    assert!(same.raw.is_empty());

    app.permissions.set(open_entry("customer"));
    let moved = permissions(
        &app,
        "?doctype=customer",
        &[("if-none-match", "\"42\"".into())],
    )
    .await;
    moved.expect_status(200, "snapshot moved");
    assert_eq!(moved.header("etag"), Some("\"43\""));
    assert!(
        moved.body["data"]["doctypes"][0]["fields"]
            .get("revealable")
            .is_none()
    );
}

#[tokio::test]
async fn doctypes_repeat_unknown_is_the_contract_error_and_none_means_all() {
    let app = App::new();
    let both = permissions(&app, "?doctype=customer&doctype=sales_invoice", &[]).await;
    assert_eq!(both.body["data"]["doctypes"].as_array().must().len(), 2);
    let all = permissions(&app, "", &[]).await;
    assert_eq!(all.body["data"]["doctypes"].as_array().must().len(), 2);
    let unknown = permissions(&app, "?doctype=nonesuch", &[]).await;
    unknown.expect_status(404, "unknown doctype");
    assert_eq!(unknown.error_code(), "meta.unknown_doctype");
    let bad = permissions(&app, "?doctype=Bad-Name", &[]).await;
    bad.expect_status(400, "bad name");
    let anonymous = harness::call(&app, "GET", "/api/v1/auth/permissions", &[], None).await;
    assert_eq!(anonymous.status, 401);
}

#[tokio::test]
async fn revealable_never_lists_a_field_that_is_not_masked() {
    let app = App::new();
    let mut entry = open_entry("customer");
    entry.fields.masked = vec!["phone".into()];
    entry.fields.revealable = vec!["phone".into(), "category".into()];
    app.permissions.set(entry);
    let call = permissions(&app, "?doctype=customer", &[]).await;
    assert_eq!(
        call.body["data"]["doctypes"][0]["fields"]["revealable"],
        json!(["phone"])
    );
}

proptest! {
    #[test]
    fn revealable_is_always_a_subset_of_masked(
        masked in proptest::collection::vec("[a-z]{1,4}", 0..6),
        revealable in proptest::collection::vec("[a-z]{1,4}", 0..6),
        hidden in proptest::collection::vec("[a-z]{1,4}", 0..3),
    ) {
        let summary = PermissionSummary {
            doctypes: vec![DoctypePermissions {
                fields: FieldSummary { hidden: hidden.clone(), read_only: vec![], masked, revealable },
                ..open_entry("customer")
            }],
            version: 1,
        };
        let json = summary.to_json().must();
        let fields = &json["data"]["doctypes"][0]["fields"];
        let masked: Vec<_> = fields["masked"].as_array().must().clone();
        if let Some(revealable) = fields.get("revealable") {
            for name in revealable.as_array().must() {
                prop_assert!(masked.contains(name));
            }
        }
        for name in &hidden {
            prop_assert!(!masked.contains(&json!(name)));
        }
    }

    #[test]
    fn generated_field_paths_are_paths(
        table in "[a-z][a-z0-9_]{0,10}",
        idx in 1u32..10_000,
        field in "[a-z][a-z0-9_]{0,10}",
    ) {
        prop_assert!(is_field_path(&table));
        let row_path = format!("{table}[{idx}].{field}");
        prop_assert!(is_field_path(&row_path));
        let zero_idx = format!("{table}[0].{field}");
        prop_assert!(!is_field_path(&zero_idx));
        let upper = table.to_uppercase();
        let shouted = format!("{upper}A");
        prop_assert!(!is_field_path(&shouted));
    }
}

// ---------------------------------------------------------------------------
// Reveal

fn rid() -> RecordId {
    RecordId::new_v7()
}

async fn reveal(
    app: &App,
    token: &str,
    id: RecordId,
    body: serde_json::Value,
    extra: &[(&str, String)],
) -> harness::Call {
    let mut headers = vec![harness::bearer(token)];
    headers.extend_from_slice(extra);
    harness::call(
        app,
        "POST",
        &format!("/api/v1/resource/customer/{id}/reveal"),
        &headers,
        Some(body),
    )
    .await
}

fn good_body() -> serde_json::Value {
    json!({ "field": "phone", "reason": "Customer asked on ticket 1." })
}

#[tokio::test]
async fn reveal_returns_the_value_with_no_store_and_validates_its_body() {
    let app = App::new();
    let id = rid();
    app.reveal.grant(TOKEN_ALICE);
    app.reveal.add_document(id);
    app.reveal
        .allow_field("customer", "phone", json!("0300-0000000"));
    let ok = reveal(&app, TOKEN_ALICE, id, good_body(), &[]).await;
    ok.expect_status(200, "reveal");
    assert_eq!(ok.body["data"]["value"], "0300-0000000");
    assert_eq!(ok.header("cache-control"), Some("no-store"));
    for bad in [
        json!({ "field": "phone" }),
        json!({ "field": "phone", "reason": "" }),
        json!({ "field": "phone", "reason": "x".repeat(501) }),
        json!({ "field": "Phone", "reason": "x" }),
        json!({ "fields": ["phone"], "reason": "x" }),
    ] {
        let call = reveal(&app, TOKEN_ALICE, id, bad, &[]).await;
        call.expect_status(400, "closed request");
    }
    // Only the one good call reached the service.
    assert_eq!(app.reveal.calls().len(), 1);
}

#[tokio::test]
async fn reveal_is_never_replayed_from_the_idempotency_store() {
    let app = App::new();
    let id = rid();
    app.reveal.grant(TOKEN_ALICE);
    app.reveal.add_document(id);
    app.reveal.allow_field("customer", "phone", json!("0300"));
    let key = [harness::idem_key("reveal-key-0001")];
    let first = reveal(&app, TOKEN_ALICE, id, good_body(), &key).await;
    let second = reveal(&app, TOKEN_ALICE, id, good_body(), &key).await;
    first.expect_status(200, "first");
    second.expect_status(200, "second");
    assert!(second.header("idempotency-replayed").is_none());
    let calls = app.reveal.calls();
    assert_eq!(calls.len(), 2, "the same key reaches the service twice");
    assert!(calls.iter().all(|c| c.idempotency_key.is_none()));
    assert_eq!(calls[0].reason, "Customer asked on ticket 1.");
}

#[tokio::test]
async fn the_four_refusal_causes_answer_the_same_body() {
    let app = App::new();
    let (readable, unreadable, missing) = (rid(), rid(), rid());
    app.reveal.add_document(readable);
    app.reveal.add_unreadable(unreadable);
    app.reveal
        .allow_field("customer", "phone", json!("secret-value"));
    // No right: Alice does not hold it.
    let no_right = reveal(&app, TOKEN_ALICE, readable, good_body(), &[]).await;
    app.reveal.grant(TOKEN_ALICE);
    // Not revealable.
    let not_revealable = reveal(
        &app,
        TOKEN_ALICE,
        readable,
        json!({ "field": "category", "reason": "x" }),
        &[],
    )
    .await;
    let no_document = reveal(&app, TOKEN_ALICE, missing, good_body(), &[]).await;
    let forbidden = reveal(&app, TOKEN_ALICE, unreadable, good_body(), &[]).await;
    let mut bodies = Vec::new();
    for call in [&no_right, &not_revealable, &no_document, &forbidden] {
        call.expect_status(403, "refusal");
        assert_eq!(call.error_code(), "permission.reveal_denied");
        let mut body = call.body.clone();
        body["error"]["request_id"] = json!("-");
        bodies.push(body);
    }
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "{bodies:?}"
    );
    assert!(!no_right.raw.contains("secret-value"));
}

/// Captures the text of every tracing event, so a test can prove a value never reaches a log.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Visit<'a>(&'a mut String);

impl tracing::field::Visit for Visit<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if let Ok(mut text) = self.0.lock() {
            event.record(&mut Visit(&mut text));
            text.push('\n');
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn the_revealed_value_and_reason_never_reach_a_log_line() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let app = App::new();
    let id = rid();
    app.reveal.grant(TOKEN_ALICE);
    app.reveal.add_document(id);
    app.reveal
        .allow_field("customer", "phone", json!("TOP-SECRET-0042"));
    let ok = reveal(&app, TOKEN_ALICE, id, good_body(), &[]).await;
    ok.expect_status(200, "reveal");
    let refused = reveal(&app, TOKEN_ALICE, rid(), good_body(), &[]).await;
    refused.expect_status(403, "refusal");
    let logged = capture.0.lock().must().clone();
    assert!(!logged.contains("TOP-SECRET-0042"), "log: {logged}");
    assert!(!refused.raw.contains("TOP-SECRET-0042"));
}

// ---------------------------------------------------------------------------
// `_currency` is presentation and never accepted on a write

#[tokio::test]
async fn currency_is_rejected_on_create_update_and_patch_and_never_a_fingerprint() {
    let app = App::new();
    let token = TOKEN_ALICE;
    app.fake.set_currency("name", "USD", "$");
    let created = harness::create_doc(&app, token, "customer", json!({ "name": "Cur A" })).await;
    created.expect_status(201, "create");
    assert_eq!(created.body["data"]["_currency"]["name"]["code"], "USD");
    let bad_create = harness::create_doc(
        &app,
        token,
        "customer",
        json!({ "name": "Cur B", "_currency": {} }),
    )
    .await;
    bad_create.expect_status(400, "create with _currency");
    assert_eq!(bad_create.error_code(), "api.invalid_request");
    let id = created.doc_id();
    let patched = harness::patch_doc(
        &app,
        token,
        "customer",
        &id,
        &[harness::if_match(created.doc_version())],
        json!({ "_currency": { "name": { "code": "EUR", "symbol": "E" } } }),
    )
    .await;
    patched.expect_status(400, "patch with _currency");
    assert_eq!(patched.error_code(), "api.invalid_request");
    // The version did not move: the rejected body wrote nothing.
    let after = harness::get_doc(&app, token, "customer", &id).await;
    assert_eq!(after.doc_version(), created.doc_version());
}

// ---------------------------------------------------------------------------
// A 0.4.0-shaped service round-trips unchanged

#[tokio::test]
async fn a_service_with_none_of_the_new_properties_changes_nothing() {
    let app = App::new();
    let token = TOKEN_ALICE;
    let created = harness::seed_customer(&app, token, "Plain").await;
    created.expect_status(201, "seed");
    let id = created.doc_id();
    let doc = harness::get_doc(&app, token, "customer", &id).await;
    assert!(doc.body["data"].get("_currency").is_none());
    let meta = harness::get_meta(&app, token, "customer", &[]).await;
    let text = meta.raw.clone();
    assert!(!text.contains("\"multiple\"") && !text.contains("\"control\""));
    let page = harness::call(
        &app,
        "POST",
        "/api/v1/lookup/customer/name",
        &[harness::bearer(token)],
        Some(json!({ "text": "Plain" })),
    )
    .await;
    page.expect_status(200, "lookup");
    assert!(page.body["data"].get("columns").is_none());
    assert!(page.body["data"]["rows"][0].get("cells").is_none());
}
