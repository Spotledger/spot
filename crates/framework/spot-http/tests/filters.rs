//! The `Filter` JSON over the wire (brief WP-PLT-05.T3, suite "Filters"): valid trees query
//! correctly, every rejection carries its code, and the depth (16), node (200) and `in`
//! (1,000) limits hold.

#[allow(dead_code)]
mod support;

use serde_json::Value as Json;
use support::Must;
use support::fake::TOKEN_ALICE;
use support::harness::{self, App};

/// A catalog seeded for the filters: three customers and two doc_notes with mixed values.
async fn seed(app: &App) {
    let alice = TOKEN_ALICE;
    for (name, category, phone) in [
        ("Aurora", "retail", "0301-111"),
        ("Borealis", "wholesale", "0302-222"),
        ("Corona", "retail", "0303-333"),
    ] {
        let call = harness::create_doc(
            app,
            alice,
            "customer",
            serde_json::json!({ "name": name, "category": category, "phone": phone }),
        )
        .await;
        call.expect_status(201, "seed customer");
    }
    for (name, count, price) in [("N1", 5, "1.25"), ("N2", -1, "0.50")] {
        let call = harness::create_doc(
            app,
            alice,
            "doc_note",
            serde_json::json!({ "name": name, "count": count, "price": price }),
        )
        .await;
        call.expect_status(201, "seed doc_note");
    }
}

/// POSTs a query body and returns the page.
async fn query(app: &App, doctype: &str, body: Json) -> harness::Call {
    harness::call(
        app,
        "POST",
        &format!("/api/v1/resource/{doctype}/query"),
        &[harness::bearer(TOKEN_ALICE)],
        Some(body),
    )
    .await
}

/// The page's documents.
fn data_of(call: &harness::Call) -> Vec<Json> {
    call.body["data"].as_array().must().clone()
}

#[tokio::test]
async fn valid_trees_filter_and_order() {
    let app = App::new();
    seed(&app).await;

    // eq on text.
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "filter": { "field": "category", "op": "eq", "value": "retail" } }),
    )
    .await;
    probe.expect_status(200, "eq filter");
    assert_eq!(data_of(&probe).len(), 2, "Aurora and Corona");
    // in with several values.
    let probe = query(&app, "customer", serde_json::json!({ "filter": { "field": "category", "op": "in", "value": ["retail", "wholesale"] } })).await;
    probe.expect_status(200, "in filter");
    assert_eq!(data_of(&probe).len(), 3);
    // ne.
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "filter": { "field": "category", "op": "ne", "value": "retail" } }),
    )
    .await;
    assert_eq!(data_of(&probe).len(), 1, "only Borealis");
    // prefix / contains, case-insensitive.
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "filter": { "field": "name", "op": "prefix", "value": "Aur" } }),
    )
    .await;
    assert_eq!(data_of(&probe).len(), 1);
    let probe = query(&app, "customer", serde_json::json!({ "filter": { "field": "name", "op": "contains", "value": "ORE", "case_insensitive": true } })).await;
    assert_eq!(data_of(&probe).len(), 1, "Borealis case-insensitively");
    // is_null / is_not_null.
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "filter": { "field": "category", "op": "is_not_null" } }),
    )
    .await;
    assert_eq!(data_of(&probe).len(), 3);
    let probe = query(
        &app,
        "doc_note",
        serde_json::json!({ "filter": { "field": "flag", "op": "is_null" } }),
    )
    .await;
    assert_eq!(data_of(&probe).len(), 2, "flag was never set");
    // and/or/not combinations, on ints.
    let probe = query(
        &app,
        "doc_note",
        serde_json::json!({ "filter": {
        "and": [
            { "field": "count", "op": "ge", "value": 0 },
            { "or": [
                { "field": "name", "op": "eq", "value": "N1" },
                { "not": { "field": "name", "op": "eq", "value": "N1" } }
            ] }
        ]
    } }),
    )
    .await;
    probe.expect_status(200, "nested tree");
    assert_eq!(data_of(&probe).len(), 1, "N1 only (count >= 0)");
    // int comparison.
    let probe = query(
        &app,
        "doc_note",
        serde_json::json!({ "filter": { "field": "count", "op": "lt", "value": 0 } }),
    )
    .await;
    assert_eq!(data_of(&probe).len(), 1, "N2 has count -1");
    // No match at all.
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "filter": { "field": "name", "op": "eq", "value": "nobody" } }),
    )
    .await;
    probe.expect_status(200, "an empty page is still a valid page");
    assert_eq!(data_of(&probe).len(), 0, "no rows match");
}

#[tokio::test]
async fn ordering_limit_cursor_fields_and_total() {
    let app = App::new();
    seed(&app).await;

    // Newest first by default (creation order, newest last stored).
    let probe = query(&app, "customer", serde_json::json!({})).await;
    assert_eq!(
        data_of(&probe)[0]["name"].as_str().must(),
        "Corona",
        "newest first"
    );
    // desc order on name.
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "order": [ { "field": "name", "dir": "desc" } ] }),
    )
    .await;
    let data = data_of(&probe);
    assert_eq!(data[0]["name"].as_str().must(), "Corona");
    assert_eq!(data[2]["name"].as_str().must(), "Aurora");
    // asc order.
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "order": [ { "field": "name", "dir": "asc", "nulls": "last" } ] }),
    )
    .await;
    assert_eq!(data_of(&probe)[0]["name"].as_str().must(), "Aurora");
    // limit + cursor paging.
    let probe = query(&app, "customer", serde_json::json!({ "limit": 2 })).await;
    assert_eq!(data_of(&probe).len(), 2);
    assert!(probe.body["total"].is_null(), "no total without with_total");
    let cursor = probe.body["next_cursor"].as_str().must().to_owned();
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "limit": 2, "cursor": cursor }),
    )
    .await;
    assert_eq!(data_of(&probe).len(), 1, "the third row");
    assert!(
        probe.body["next_cursor"].is_null(),
        "the last page has no cursor"
    );
    // fields projection keeps id and version.
    let probe = query(&app, "customer", serde_json::json!({ "fields": ["name"] })).await;
    let first = data_of(&probe)[0].clone();
    assert!(
        first.get("category").is_none(),
        "unrequested fields are absent"
    );
    assert!(
        first.get("id").is_some() && first.get("version").is_some(),
        "id and version stay"
    );
    // with_total.
    let probe = query(
        &app,
        "customer",
        serde_json::json!({ "with_total": true, "limit": 2 }),
    )
    .await;
    assert_eq!(
        probe.body["total"].as_u64().must(),
        3,
        "the total is the match count"
    );
    // The list endpoint's query string: fields, limit, cursor.
    let probe = harness::call(
        &app,
        "GET",
        "/api/v1/resource/customer?limit=2&fields=name",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    probe.expect_status(200, "list with fields and limit");
    assert_eq!(data_of(&probe).len(), 2);
    assert!(data_of(&probe)[0].get("category").is_none());
}

/// Asserts a query rejection: the registry status and the given code.
fn assert_rejection(probe: &harness::Call, code: &str, what: &str) {
    let entry = spot_http::api::registry::entries()
        .into_iter()
        .find(|(c, _, _, _)| *c == code)
        .unwrap_or_else(|| panic!("{code} is not a registry code"));
    probe.expect_status(entry.1, what);
    assert_eq!(probe.error_code(), code, "{what}");
}

#[tokio::test]
async fn every_rejection_carries_its_code() {
    let app = App::new();
    seed(&app).await;

    // Unknown field in a comparison.
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "filter": { "field": "nme", "op": "eq", "value": "x" } }),
        )
        .await,
        "document.unknown_field",
        "unknown filter field",
    );
    // Unknown field in an order key.
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "order": [ { "field": "nme" } ] }),
        )
        .await,
        "document.unknown_field",
        "unknown order field",
    );
    // A text operator on an int field: the operator does not fit the type.
    assert_rejection(
        &query(
            &app,
            "doc_note",
            serde_json::json!({ "filter": { "field": "count", "op": "contains", "value": "1" } }),
        )
        .await,
        "api.invalid_request",
        "contains on int",
    );
    // A comparison value that does not fit the field type (text for an int).
    assert_rejection(
        &query(
            &app,
            "doc_note",
            serde_json::json!({ "filter": { "field": "count", "op": "eq", "value": "five" } }),
        )
        .await,
        "api.invalid_request",
        "text for an int field",
    );
    // A decimal string that is not a decimal.
    assert_rejection(
        &query(
            &app,
            "doc_note",
            serde_json::json!({ "filter": { "field": "price", "op": "eq", "value": "1e5" } }),
        )
        .await,
        "api.invalid_request",
        "exponent decimal",
    );
    // A text comparison with an empty value.
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "filter": { "field": "name", "op": "prefix", "value": "" } }),
        )
        .await,
        "api.invalid_request",
        "empty prefix",
    );
    // Unknown operator.
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "filter": { "field": "name", "op": "like", "value": "x" } }),
        )
        .await,
        "api.invalid_request",
        "unknown operator",
    );
    // A null value: three-valued logic means `is_null`, never `eq null`.
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "filter": { "field": "category", "op": "eq", "value": null } }),
        )
        .await,
        "api.invalid_request",
        "eq null",
    );
    // An empty and/or; a non-object node; a node with two combinators.
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "filter": { "and": [] } }),
        )
        .await,
        "api.invalid_request",
        "empty and",
    );
    assert_rejection(
        &query(&app, "customer", serde_json::json!({ "filter": [1] })).await,
        "api.invalid_request",
        "an array node",
    );
    assert_rejection(
        &query(&app, "customer", serde_json::json!({ "filter": { "and": [ { "field": "name", "op": "eq", "value": "x" } ], "not": { "field": "name", "op": "eq", "value": "x" } } })).await,
        "api.invalid_request",
        "and + not in one node",
    );
    // Bad direction and bad nulls.
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "order": [ { "field": "name", "dir": "up" } ] }),
        )
        .await,
        "api.invalid_request",
        "bad dir",
    );
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "order": [ { "field": "name", "nulls": "middle" } ] }),
        )
        .await,
        "api.invalid_request",
        "bad nulls",
    );
    // Extra query keys (the contract's additionalProperties: false).
    assert_rejection(
        &query(&app, "customer", serde_json::json!({ "where": {} })).await,
        "api.invalid_request",
        "unknown query key",
    );
    // limit bounds.
    assert_rejection(
        &query(&app, "customer", serde_json::json!({ "limit": 0 })).await,
        "api.invalid_request",
        "limit 0",
    );
    assert_rejection(
        &query(&app, "customer", serde_json::json!({ "limit": 501 })).await,
        "api.invalid_request",
        "limit 501",
    );
    // A cursor that did not come from this store.
    assert_rejection(
        &query(
            &app,
            "customer",
            serde_json::json!({ "cursor": "not-a-cursor" }),
        )
        .await,
        "api.invalid_request",
        "bad cursor",
    );
}

#[tokio::test]
async fn depth_node_and_in_limits_hold() {
    let app = App::new();
    seed(&app).await;

    // Depth: 15 nested nots (16 levels) are fine; one more is api.invalid_request.
    let nested = |levels: usize| {
        let mut node = serde_json::json!({ "field": "name", "op": "eq", "value": "Aurora" });
        for _ in 0..levels {
            node = serde_json::json!({ "not": node });
        }
        serde_json::json!({ "filter": node })
    };
    let probe = query(&app, "customer", nested(15)).await;
    probe.expect_status(200, "16 levels total are inside the depth limit");
    let probe = query(&app, "customer", nested(16)).await;
    assert_rejection(&probe, "api.invalid_request", "17 levels deep");

    // Nodes: a 200-node and-chain is fine; one more is rejected.
    let many = |leaves: usize| {
        let list: Vec<Json> = (0..leaves)
            .map(|_| serde_json::json!({ "field": "name", "op": "eq", "value": "Aurora" }))
            .collect();
        serde_json::json!({ "filter": { "and": list } })
    };
    let probe = query(&app, "customer", many(199)).await;
    probe.expect_status(200, "199 leaves + 1 and node = 200 nodes");
    let probe = query(&app, "customer", many(200)).await;
    assert_rejection(&probe, "api.invalid_request", "201 nodes");

    // `in`: 1,000 values are the limit; 1,001 are rejected.
    let values: Vec<Json> = (0..1_000u32).map(|n| Json::from(i64::from(n))).collect();
    let probe = query(
        &app,
        "doc_note",
        serde_json::json!({ "filter": { "field": "count", "op": "in", "value": values } }),
    )
    .await;
    probe.expect_status(200, "1,000 in-values are the limit");
    let values: Vec<Json> = (0..1_001u32).map(|n| Json::from(i64::from(n))).collect();
    let probe = query(
        &app,
        "doc_note",
        serde_json::json!({ "filter": { "field": "count", "op": "in", "value": values } }),
    )
    .await;
    assert_rejection(&probe, "api.invalid_request", "1,001 in-values");
    // An empty in is not a filter over nothing: rejected.
    assert_rejection(
        &query(
            &app,
            "doc_note",
            serde_json::json!({ "filter": { "field": "count", "op": "in", "value": [] } }),
        )
        .await,
        "api.invalid_request",
        "empty in",
    );

    // Order keys: 8 are the max (the contract's maxItems).
    let keys: Vec<Json> = ["name", "category", "phone"]
        .iter()
        .cycle()
        .take(8)
        .map(|field| serde_json::json!({ "field": field }))
        .collect();
    let probe = query(&app, "customer", serde_json::json!({ "order": keys })).await;
    probe.expect_status(200, "8 order keys are the max");
}
