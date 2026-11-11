#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use support::App;

#[tokio::test]
async fn keyset_pages_cover_ties_nulls_both_directions_and_null_placements() {
    for dir in ["asc", "desc"] {
        for nulls in ["first", "last"] {
            let app = App::new().await;
            for (title, score) in [
                ("A", Some(2)),
                ("B", None),
                ("C", Some(2)),
                ("D", Some(1)),
                ("E", None),
            ] {
                app.call(
                    "POST",
                    "/api/v1/resource/note",
                    &[],
                    Some(json!({"title":title,"score":score})),
                )
                .await
                .status(201);
            }
            let base = json!({"fields":["title","score"],"order":[{"field":"score","dir":dir,"nulls":nulls}],"limit":2,"with_total":true});
            let mut all = Vec::new();
            let mut request = base.clone();
            for _ in 0..4 {
                let r = app
                    .call(
                        "POST",
                        "/api/v1/resource/note/query",
                        &[],
                        Some(request.clone()),
                    )
                    .await;
                r.status(200);
                assert_eq!(r.body["total"], 5);
                all.extend(r.body["data"].as_array().unwrap().clone());
                match r.body["next_cursor"].as_str() {
                    Some(cursor) => request["cursor"] = json!(cursor),
                    None => break,
                }
            }
            assert_eq!(all.len(), 5, "{dir}/{nulls}: {all:?}");
            let ids: std::collections::BTreeSet<_> =
                all.iter().map(|d| d["id"].as_str().unwrap()).collect();
            assert_eq!(ids.len(), 5);
            let scores: Vec<_> = all.iter().map(|d| d["score"].as_i64()).collect();
            let mut expected = if dir == "asc" {
                vec![Some(1), Some(2), Some(2)]
            } else {
                vec![Some(2), Some(2), Some(1)]
            };
            if nulls == "first" {
                expected.splice(0..0, [None, None]);
            } else {
                expected.extend([None, None]);
            }
            assert_eq!(scores, expected, "{dir}/{nulls}");
        }
    }
}

#[tokio::test]
async fn deleting_a_previous_row_does_not_skip_the_next_page() {
    let app = App::new().await;
    let first = app.create("First").await;
    let second = app.create("Second").await;
    let third = app.create("Third").await;
    let page = app
        .call("GET", "/api/v1/resource/note?limit=1", &[], None)
        .await;
    page.status(200);
    assert_eq!(page.body["data"][0]["id"], first.id());
    app.call("DELETE", &first.path(), &[("if-match", first.etag())], None)
        .await
        .status(204);
    let next = app
        .call(
            "GET",
            &format!(
                "/api/v1/resource/note?limit=1&cursor={}",
                page.body["next_cursor"].as_str().unwrap()
            ),
            &[],
            None,
        )
        .await;
    next.status(200);
    assert_eq!(next.body["data"][0]["id"], second.id());
    let last = app
        .call(
            "GET",
            &format!(
                "/api/v1/resource/note?limit=1&cursor={}",
                next.body["next_cursor"].as_str().unwrap()
            ),
            &[],
            None,
        )
        .await;
    last.status(200);
    assert_eq!(last.body["data"][0]["id"], third.id());
    assert!(last.body["next_cursor"].is_null());
}

#[tokio::test]
async fn cursor_is_bound_to_query_and_malformed_values_are_not_silently_offsets() {
    let app = App::new().await;
    app.create("A").await;
    app.create("B").await;
    let first = app
        .call(
            "POST",
            "/api/v1/resource/note/query",
            &[],
            Some(json!({"limit":1})),
        )
        .await;
    first.status(200);
    for req in [
        json!({"limit":1,"cursor":"p:20"}),
        json!({"limit":1,"cursor":"not-base64!"}),
        json!({"limit":1,"fields":["title"],"cursor":first.body["next_cursor"]}),
    ] {
        let bad = app
            .call("POST", "/api/v1/resource/note/query", &[], Some(req))
            .await;
        bad.status(400);
        assert_eq!(bad.body["error"]["code"], "api.invalid_request");
    }
}

#[tokio::test]
async fn every_filter_operator_executes_through_storage_ir() {
    let app = App::new().await;
    app.call(
        "POST",
        "/api/v1/resource/note",
        &[],
        Some(json!({"title":"Ab_%z", "score":2})),
    )
    .await
    .status(201);
    app.call(
        "POST",
        "/api/v1/resource/note",
        &[],
        Some(json!({"title":"Other", "score":5})),
    )
    .await
    .status(201);
    let cases = [
        (json!({"field":"score","op":"eq","value":2}), 1),
        (json!({"field":"score","op":"ne","value":2}), 1),
        (json!({"field":"score","op":"lt","value":5}), 1),
        (json!({"field":"score","op":"le","value":2}), 1),
        (json!({"field":"score","op":"gt","value":2}), 1),
        (json!({"field":"score","op":"ge","value":5}), 1),
        (json!({"field":"score","op":"in","value":[2,5]}), 2),
        (json!({"field":"day","op":"is_null"}), 2),
        (json!({"field":"day","op":"is_not_null"}), 0),
        (
            json!({"field":"title","op":"prefix","value":"ab","case_insensitive":true}),
            1,
        ),
        (json!({"field":"title","op":"contains","value":"_%"}), 1),
        (json!({"field":"title","op":"suffix","value":"z"}), 1),
        (
            json!({"or":[{"field":"score","op":"eq","value":2},{"field":"score","op":"eq","value":5}]}),
            2,
        ),
    ];
    for (filter, count) in cases {
        let r = app
            .call(
                "POST",
                "/api/v1/resource/note/query",
                &[],
                Some(json!({"filter":filter,"with_total":true})),
            )
            .await;
        r.status(200);
        assert_eq!(
            r.body["data"].as_array().unwrap().len(),
            count,
            "{filter}: {}",
            r.body
        );
        assert_eq!(r.body["total"], json!(count));
    }
}

#[tokio::test]
async fn invalid_filter_and_missing_precondition_never_write() {
    let app = App::new().await;
    let doc = app.create("Untouched").await;
    for filter in [
        json!({"field":"unknown","op":"eq","value":1}),
        json!({"field":"amount","op":"eq","value":1}),
    ] {
        let r = app
            .call(
                "POST",
                "/api/v1/resource/note/query",
                &[],
                Some(json!({"filter":filter})),
            )
            .await;
        assert!(matches!(r.status, 400 | 422), "{}", r.body);
    }
    app.call("PATCH", &doc.path(), &[], Some(json!({"title":"Changed"})))
        .await
        .status(428);
    let loaded = app.call("GET", &doc.path(), &[], None).await;
    assert_eq!(loaded.body["data"]["title"], "Untouched");
    assert_eq!(loaded.etag(), doc.etag());
}

#[test]
fn compiled_queries_never_use_offsets_and_always_project_id() {
    let mut def =
        spot_meta::Definition::parse("note_row.toml", include_str!("fixtures/note_row.toml"))
            .unwrap();
    def.is_child = false;
    let effective = spot_meta::Registry::new(vec![def]).unwrap().effective();
    let meta_service = spot_api_services::MetaResources::new(effective);
    // Compile the native HTTP metadata built from the real effective definition.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ctx = spot_http::api::service::RequestCtx {
        site: spot_core::SiteId::new("test").unwrap(),
        credential: None,
        idempotency_key: None,
        language: None,
        request_id: spot_http::RequestId::generate(),
        peer: None,
    };
    use spot_http::api::service::MetaService;
    let meta = rt
        .block_on(meta_service.doctype_meta(&ctx, "note_row"))
        .unwrap();
    let spec = spot_http::api::service::QuerySpec {
        fields: vec![],
        filter: None,
        order: vec![],
        limit: 2,
        cursor: None,
        with_total: false,
    };
    let q = spot_api_services::query::compile(&meta, &spec).unwrap();
    assert_eq!(q.offset, 0);
    assert_eq!(q.limit, Some(3));
    assert_eq!(q.select[0].alias.as_str(), "id");
    assert_eq!(q.order_by.len(), 1);
    let _ = Json::Null;
}
