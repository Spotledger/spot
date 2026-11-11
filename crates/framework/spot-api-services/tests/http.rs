#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_storage_contract::{ChangeFeed, FeedPosition};
use support::{App, Call};

#[tokio::test]
async fn create_maps_invocation_messages_and_idempotency_key_to_real_lifecycle_event() {
    let app = App::new().await;
    let key = [("idempotency-key", "glm-357e-create".into())];
    let r = app
        .call(
            "POST",
            "/api/v1/resource/note",
            &key,
            Some(json!({"title":"First", "amount":"12.30", "rows":[{"text":"child"}]})),
        )
        .await;
    r.status(201);
    assert_eq!(r.body["data"]["amount"], "12.30");
    assert_eq!(r.body["data"]["rows"][0]["text"], "child");
    assert_eq!(
        r.body["messages"][0]["message"],
        "Created by real lifecycle"
    );
    // The journal's `doc_state` images (WP-JRN-02.T2) are not the lifecycle events counted here.
    let events: Vec<_> = app
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.event.topic.as_str() != "doc_state")
        .collect();
    assert_eq!(events.len(), 1);
    let event: Json = serde_json::from_str(&events[0].event.payload).unwrap();
    assert_eq!(event["meta"]["command_id"], "glm-357e-create", "{event}");
    let replay = app
        .call(
            "POST",
            "/api/v1/resource/note",
            &key,
            Some(json!({"title":"First", "amount":"12.30", "rows":[{"text":"child"}]})),
        )
        .await;
    replay.status(201);
    assert_eq!(replay.body, r.body);
    assert_eq!(
        app.engine
            .backend()
            .read_outbox(FeedPosition::START, 100)
            .await
            .unwrap()
            .iter()
            .filter(|r| r.event.topic.as_str() != "doc_state")
            .count(),
        1
    );
}

#[tokio::test]
async fn get_loads_saved_document_and_etag() {
    let app = App::new().await;
    let created = app.create("Get").await;
    let r = app.call("GET", &created.path(), &[], None).await;
    r.status(200);
    for (key, value) in created.body["data"].as_object().unwrap() {
        assert_eq!(&r.body["data"][key], value);
    }
    assert_eq!(r.etag(), created.etag());
}

#[tokio::test]
async fn update_preserves_absent_values_clears_tables_and_checks_version() {
    let app = App::new().await;
    let created = app
        .call(
            "POST",
            "/api/v1/resource/note",
            &[],
            Some(json!({"title":"Before", "score":7,"rows":[{"text":"old"}]})),
        )
        .await;
    created.status(201);
    let r = app
        .call(
            "PATCH",
            &created.path(),
            &[("if-match", created.etag())],
            Some(json!({"title":"After", "rows":[]})),
        )
        .await;
    r.status(200);
    assert_eq!(r.body["data"]["title"], "After");
    assert_eq!(r.body["data"]["score"], 7);
    assert_eq!(r.body["data"]["rows"], json!([]));
    assert_ne!(r.etag(), created.etag());
    let stale = app
        .call(
            "PATCH",
            &created.path(),
            &[("if-match", created.etag())],
            Some(json!({"title":"Lost"})),
        )
        .await;
    stale.status(412);
    assert_eq!(stale.body["error"]["code"], "document.stale_version");
    assert!(stale.body["error"]["details"].is_object(), "{}", stale.body);
}

#[tokio::test]
async fn delete_removes_real_document() {
    let app = App::new().await;
    let created = app.create("Delete").await;
    app.call(
        "DELETE",
        &created.path(),
        &[("if-match", created.etag())],
        None,
    )
    .await
    .status(204);
    let missing = app.call("GET", &created.path(), &[], None).await;
    missing.status(404);
    assert_eq!(missing.body["error"]["code"], "document.not_found");
}

#[tokio::test]
async fn metadata_exposes_effective_version_and_conditional_etag() {
    let app = App::new().await;
    let r = app.call("GET", "/api/v1/meta/note", &[], None).await;
    r.status(200);
    assert_eq!(r.etag(), format!("\"{}\"", r.body["data"]["version"]));
    let fields = r.body["data"]["fields"].as_array().unwrap();
    let amount = fields.iter().find(|f| f["name"] == "amount").unwrap();
    assert_eq!(amount["default"], "0.00");
    assert_eq!(amount["scale"], 2);
    app.call(
        "GET",
        "/api/v1/meta/note",
        &[("if-none-match", r.etag())],
        None,
    )
    .await
    .status(304);
}

#[tokio::test]
async fn list_returns_real_documents_without_needing_id_in_projection() {
    let app = App::new().await;
    let created = app.create("List").await;
    let r = app
        .call(
            "GET",
            "/api/v1/resource/note?fields=title&limit=1",
            &[],
            None,
        )
        .await;
    r.status(200);
    assert_eq!(r.body["data"][0]["id"], created.id(), "{}", r.body);
    assert!(r.body.get("total").is_none());
    assert!(r.body["data"][0].get("amount").is_none());
}

#[tokio::test]
async fn query_converts_decimal_date_and_boolean_ast_and_returns_total() {
    let app = App::new().await;
    app.call(
        "POST",
        "/api/v1/resource/note",
        &[],
        Some(json!({"title":"Match", "amount":"12.30", "day":"2026-10-15"})),
    )
    .await
    .status(201);
    app.create("Other").await;
    let r = app.call("POST", "/api/v1/resource/note/query", &[], Some(json!({"filter":{"and":[{"field":"amount","op":"eq","value":"12.30"},{"not":{"field":"day","op":"lt","value":"2026-10-15"}}]},"with_total":true}))).await;
    r.status(200);
    assert_eq!(r.body["data"].as_array().unwrap().len(), 1, "{}", r.body);
    assert_eq!(r.body["data"][0]["title"], "Match");
    assert_eq!(r.body["total"], 1);
}

async fn act(app: &App, path: &str, action: &str, etag: String, body: Option<Json>) -> Call {
    app.call(
        "POST",
        &format!("{path}/{action}"),
        &[("if-match", etag)],
        body,
    )
    .await
}

#[tokio::test]
async fn post_then_cancel_change_the_state_and_the_version() {
    let app = App::new().await;
    let created = app.create("Lifecycle").await;
    let posted = act(&app, &created.path(), "post", created.etag(), None).await;
    posted.status(200);
    assert_eq!(posted.body["data"]["_state"], "posted", "{}", posted.body);
    let again = act(&app, &created.path(), "post", posted.etag(), None).await;
    again.status(409);
    assert_eq!(again.body["error"]["code"], "document.invalid_state");
    let cancelled = act(&app, &created.path(), "cancel", posted.etag(), None).await;
    cancelled.status(200);
    assert_eq!(cancelled.body["data"]["_state"], "cancelled");
    let stale = act(&app, &created.path(), "cancel", posted.etag(), None).await;
    stale.status(412);
    assert_eq!(stale.body["error"]["code"], "document.stale_version");
}

#[tokio::test]
async fn correct_cancels_the_original_and_returns_its_amended_draft() {
    let app = App::new().await;
    let created = app.create("Original").await;
    let posted = act(&app, &created.path(), "post", created.etag(), None).await;
    posted.status(200);
    let amended = act(&app, &created.path(), "correct", posted.etag(), None).await;
    amended.status(201);
    assert_eq!(amended.body["data"]["_state"], "draft");
    assert_eq!(amended.body["data"]["amended_from"], created.id());
    assert_ne!(amended.id(), created.id());
    let original = app.call("GET", &created.path(), &[], None).await;
    original.status(200);
    assert_eq!(original.body["data"]["_state"], "cancelled");
}

#[tokio::test]
async fn rename_sets_the_title_and_a_stale_version_changes_nothing() {
    let app = App::new().await;
    let created = app.create("Before").await;
    let renamed = act(
        &app,
        &created.path(),
        "rename",
        created.etag(),
        Some(json!({"name":"Renamed"})),
    )
    .await;
    renamed.status(200);
    assert_eq!(renamed.body["data"]["title"], "Renamed");
    let stale = act(
        &app,
        &created.path(),
        "rename",
        created.etag(),
        Some(json!({"name":"Other"})),
    )
    .await;
    stale.status(412);
    let loaded = app.call("GET", &created.path(), &[], None).await;
    assert_eq!(loaded.body["data"]["title"], "Renamed");
}

#[tokio::test]
async fn an_unknown_method_is_not_found_and_changes_nothing() {
    let app = App::new().await;
    let created = app.create("Method").await;
    let before = app.call("GET", &created.path(), &[], None).await;
    let r = act(
        &app,
        &created.path(),
        "methods/probe",
        created.etag(),
        Some(json!({"args":{}})),
    )
    .await;
    r.status(404);
    let loaded = app.call("GET", &created.path(), &[], None).await;
    assert_eq!(loaded.body["data"], before.body["data"]);
}

#[tokio::test]
async fn validation_error_keeps_field_paths_and_does_not_commit() {
    let app = App::new().await;
    let r = app
        .call(
            "POST",
            "/api/v1/resource/note",
            &[],
            Some(json!({"amount":"1.00"})),
        )
        .await;
    r.status(422);
    assert_eq!(r.body["error"]["code"], "document.validation_failed");
    assert_eq!(r.body["error"]["fields"][0]["field"], "title");
    assert!(
        app.engine
            .backend()
            .read_outbox(FeedPosition::START, 100)
            .await
            .unwrap()
            .is_empty()
    );
}
