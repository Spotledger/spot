//! The lookup endpoint's contract cases (brief WP-PLT-05.T4, suite "Lookup"): every request
//! runs through the whole router, so auth, request id and the error envelope are exercised
//! the way a mounted site sees them. The fake's default seam is the platform link search
//! over its stored documents; the named overrides stand in for app seams (ADR-068 §3).
//!
//! Cases: paging with the cursor, the document snapshot reaching the service, an override
//! seam replacing the default, a failing seam answering `lookup.seam_failed` (never the
//! default list), unauthenticated and unknown-field requests, a field with no lookup, and
//! the 0.2.0 meta schema for every kind.

#[allow(dead_code)]
mod support;

use serde_json::Value as Json;
use serde_json::json;
use support::Must;
use support::fake::{LookupOverride, TOKEN_ALICE};
use support::harness::{self, App};

use spot_http::api::lookup::LookupPage;

/// `POST /api/v1/lookup/{doctype}/{field}` with a body.
async fn lookup(
    app: &App,
    token: &str,
    doctype: &str,
    field: &str,
    body: Option<Json>,
) -> harness::Call {
    let path = format!("/api/v1/lookup/{doctype}/{field}");
    match token {
        "" => harness::call(app, "POST", &path, &[], body).await,
        _ => harness::call(app, "POST", &path, &[harness::bearer(token)], body).await,
    }
}

/// The rows of a 200 answer, as (`id`, `label`) pairs.
fn rows(call: &harness::Call) -> Vec<(String, String)> {
    call.data()["rows"]
        .as_array()
        .must()
        .iter()
        .map(|row| {
            (
                row["id"].as_str().must().to_owned(),
                row["label"].as_str().must().to_owned(),
            )
        })
        .collect()
}

/// Seeds `count` customers whose names all carry the marker, so paging has pages to walk.
async fn seed_customers(app: &App, marker: &str, count: usize) {
    for index in 1..=count {
        let call = harness::seed_customer(app, TOKEN_ALICE, &format!("{marker} {index:03}")).await;
        call.expect_status(201, "seed for lookup pages");
    }
}

/// The default seam pages over the fake's stored documents: page 1 with `has_more` and a
/// cursor, page 2 from the cursor, and a stale/empty-cursor request starting at the top.
#[tokio::test]
async fn lookup_pages_with_the_cursor() {
    let app = App::new();
    let token = TOKEN_ALICE;
    seed_customers(&app, "ACME page", 5).await;

    let first = lookup(
        &app,
        token,
        "customer",
        "name",
        Some(json!({ "text": "acme page" })),
    )
    .await;
    first.expect_status(200, "first page");
    assert_eq!(rows(&first).len(), 3, "the fake's page size");
    assert_eq!(first.data()["has_more"], json!(true), "a page follows");
    let cursor = first.data()["cursor"].as_str().must().to_owned();
    assert!(!cursor.is_empty(), "the cursor is opaque but present");

    let second = lookup(
        &app,
        token,
        "customer",
        "name",
        Some(json!({ "text": "acme page", "cursor": cursor })),
    )
    .await;
    second.expect_status(200, "second page");
    assert_eq!(second.data()["has_more"], json!(false), "five docs, 3 + 2");
    assert!(
        second.data().get("cursor").is_none(),
        "no cursor on the last page"
    );
    let seen: std::collections::BTreeSet<String> = rows(&first)
        .iter()
        .chain(rows(&second).iter())
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(
        seen.len(),
        5,
        "the two pages together list every match once"
    );

    // Labels are the target DocType's title-field values, carried through.
    for (id, label) in rows(&first) {
        assert!(
            label.to_lowercase().starts_with("acme page"),
            "{id}: {label}"
        );
    }

    // A cursor that came from nowhere is rejected as a bad request, not 502 or 500.
    let bogus = lookup(
        &app,
        token,
        "customer",
        "name",
        Some(json!({ "text": "acme", "cursor": "not-a-cursor" })),
    )
    .await;
    bogus.expect_status(400, "opaque cursor misuse is a client error");
    assert_eq!(bogus.error_code(), "api.invalid_request", "{:?}", bogus.raw);
}

/// The `doc` and `parent` snapshots pass through to the service untouched: the spy override
/// (the customer fixture's `phone` field) records the request it received and the case
/// asserts on it.
#[tokio::test]
async fn the_snapshot_reaches_the_service() {
    let app = App::new();
    let token = TOKEN_ALICE;

    app.fake
        .register_lookup_override(support::fake::SEAM_SPY, LookupOverride::Spy);
    let doc = json!({"name": "ACME spy", "order_type": "export"});
    let call = harness::call(
        &app,
        "POST",
        "/api/v1/lookup/customer/phone",
        &[harness::bearer(token)],
        Some(json!({ "text": "+92", "doc": doc })),
    )
    .await;
    call.expect_status(200, "spy seam answers");
    assert_eq!(
        call.data()["rows"][0]["label"],
        json!(support::fake::SEAM_SPY),
        "the spy marks itself"
    );
    let seen = app.fake.last_lookup_request("test:spy").must();
    assert_eq!(seen["doctype"], json!("customer"), "the DocType reached it");
    assert_eq!(seen["field"], json!("phone"), "the field reached it");
    assert_eq!(seen["text"], json!("+92"), "the text reached it");
    assert_eq!(seen["doc"], doc, "the snapshot reached it verbatim");
    assert_eq!(seen["parent"], Json::Null, "no parent was sent");
}

/// A field whose definition names an override seam is answered by that seam instead of the
/// default search; and a seam that fails answers `lookup.seam_failed` with the explanation,
/// never the default list.
#[tokio::test]
async fn an_override_seam_replaces_the_default_and_a_failure_is_explicit() {
    let app = App::new();
    let token = TOKEN_ALICE;
    seed_customers(&app, "ACME default", 2).await;

    // A static override answers from its registered page (the fake seeds the invoice's
    // `customer` field with the `test:fixed` spec).
    let page = LookupPage {
        rows: vec![spot_http::api::lookup::LookupRow {
            id: spot_storage_contract::RecordId::new_v7(),
            label: "Only from the override".to_owned(),
            description: None,
            extras: Default::default(),
            cells: Default::default(),
        }],
        has_more: false,
        cursor: None,
        columns: Vec::new(),
    };
    app.fake
        .register_lookup_override(support::fake::SEAM_FIXED, LookupOverride::Static(page));

    let call = lookup(
        &app,
        token,
        "sales_invoice",
        "customer",
        Some(json!({ "text": "anything" })),
    )
    .await;
    call.expect_status(200, "the override answers the seeded field");
    // The override label proves the default list did not answer: the default search over
    // `customer` would list the two ACME docs' names and nothing else.
    for (id, label) in rows(&call) {
        assert_eq!(
            label, "Only from the override",
            "{id}: the override's row, not the default list"
        );
    }

    // A failing seam (the item DocType's `name` carries the broken spec and nothing
    // registers for it: 502 lookup.seam_failed with an explanation, `retryable` false,
    // and none of the stored rows in the answer).
    let failing = lookup(&app, token, "item", "name", Some(json!({ "text": "" }))).await;
    failing.expect_status(502, "the failing seam");
    assert_eq!(
        failing.error_code(),
        "lookup.seam_failed",
        "{}",
        failing.raw
    );
    assert_eq!(
        failing.error()["explanation"].as_str().must(),
        "the lookup of `item.name` chose a seam no provider answers.",
        "ADR-26 explanation preserved"
    );
    assert_eq!(
        failing.error()["message"].as_str().must(),
        format!(
            "The seam `{}` is not registered.",
            support::fake::SEAM_BROKEN
        ),
        "the message names no client choice (the request has no seam parameter)"
    );
    assert_eq!(failing.error()["retryable"], json!(false));
    assert!(
        failing.body.get("data").is_none(),
        "no silent fallback to the default list: {}",
        failing.raw
    );
}

/// No credential: `auth.required` before anything runs.
#[tokio::test]
async fn an_unauthenticated_request_is_rejected() {
    let app = App::new();
    let call = lookup(
        &app,
        "",
        "customer",
        "name",
        Some(json!({ "text": "acme" })),
    )
    .await;
    call.expect_status(401, "no bearer");
    assert_eq!(call.error_code(), "auth.required", "{}", call.raw);
}

/// An unknown field answers `document.unknown_field`; a field with no lookup-able value
/// kind (a table, a secret) answers `api.invalid_request`; an unknown DocType answers
/// `meta.unknown_doctype` — each before any seam runs.
#[tokio::test]
async fn unknown_and_unlookupable_fields_are_rejected() {
    let app = App::new();
    let token = TOKEN_ALICE;

    let unknown = lookup(&app, token, "customer", "nme", Some(json!({ "text": "x" }))).await;
    unknown.expect_status(422, "a field the DocType lacks");
    assert_eq!(
        unknown.error_code(),
        "document.unknown_field",
        "{}",
        unknown.raw
    );
    assert_eq!(unknown.field_paths(), vec!["nme".to_owned()]);

    let ghost_doctype = lookup(&app, token, "nothing", "name", Some(json!({ "text": "x" }))).await;
    ghost_doctype.expect_status(404, "a DocType no host defines");
    assert_eq!(ghost_doctype.error_code(), "meta.unknown_doctype");

    // `sales_invoice.items` is a table: its value is rows of documents, not candidates.
    let table = lookup(
        &app,
        token,
        "sales_invoice",
        "items",
        Some(json!({ "text": "x" })),
    )
    .await;
    table.expect_status(400, "a table field takes no lookup");
    assert_eq!(table.error_code(), "api.invalid_request", "{}", table.raw);

    // The request can never name a seam: these bodies fail as unknown properties.
    for body in [
        json!({ "text": "x", "seam": "platform:link_search" }),
        json!({ "text": "x", "lookup": { "seam": "billing:customer_search" } }),
    ] {
        let named = lookup(&app, token, "customer", "name", Some(body.clone())).await;
        named.expect_status(400, "a client-chosen seam is an unknown property");
        assert_eq!(
            named.error_code(),
            "api.invalid_request",
            "{body}: {}",
            named.raw
        );
    }
}

/// The 0.2.0 meta schema for every kind: decimals carry scale and precision, a link its
/// `link_to`, a dynamic link its `link_type_field`, a table_multiselect its `link_field`,
/// a select its values, a text its `max_chars`, and the endpoint answers `lookup` and
/// `hints` properties exactly when the definition carries them. Every `type` string must be
/// one of the contract's `FieldType` enum.
#[tokio::test]
async fn meta_describes_every_field_per_the_020_schema() {
    let app = App::new();
    let token = TOKEN_ALICE;
    let call = harness::get_meta(&app, token, "customer", &[]).await;
    call.expect_status(200, "meta for customer");

    let fields = call.data()["fields"].as_array().must();
    assert_eq!(fields.len(), 4, "customer has four fields");
    let by_name = |name: &str| {
        fields
            .iter()
            .find(|field| field["name"] == json!(name))
            .must()
            .clone()
    };
    // A text field with a cap (`name`): max_chars and the plain format omitted.
    let name = by_name("name");
    assert_eq!(name["type"], json!("text"));
    assert_eq!(name["max_chars"], json!(140));
    assert!(name.get("format").is_none(), "plain text carries no format");
    assert_eq!(name["required"], json!(true));
    // A text field without a cap (`phone`): the phone format, no max_chars.
    let phone = by_name("phone");
    assert_eq!(phone["format"], json!("phone"));
    assert!(phone.get("max_chars").is_none());
    // The `phone` field carries the spy override, so meta names its spec (for information).
    assert_eq!(
        phone["lookup"]["seam"],
        json!(support::fake::SEAM_SPY),
        "the definition's override seam is named in meta"
    );
    assert_eq!(phone["lookup"]["version"], json!(1));
    // A field with no lookup carries no `lookup` property: the platform default is implicit.
    assert!(
        by_name("name").get("lookup").is_none(),
        "a default lookup is the platform seam and stays implicit"
    );
    assert!(
        by_name("name").get("hints").is_none(),
        "hints absent when every hint is unset"
    );
    // A decimal field (`outstanding`): scale and precision.
    let outstanding = by_name("outstanding");
    assert_eq!(outstanding["type"], json!("decimal"));
    assert_eq!(outstanding["scale"], json!(2));
    assert_eq!(outstanding["precision"], json!(18));
    // Every property the schema requires is present on every field.
    for field in fields {
        for required in ["name", "label", "type", "required", "read_only"] {
            assert!(
                field.get(required).is_some(),
                "{} lacks `{required}`: {field}",
                field["name"]
            );
        }
        // The `select` values, when present, are strings in display order; a `secret` is
        // never described with a `default` (its value never travels).
        if let Some(values) = field.get("select") {
            assert!(values.is_array(), "{field}");
        }
        if field["type"] == json!("secret") {
            assert!(field.get("default").is_none(), "{field}");
        }
    }

    // The `doc_note` DocType carries one field of every remaining kind: bool, int, decimal,
    // text, date, time, datetime, json, bytes — the meta type strings come from the
    // contract's enum.
    let note = harness::get_meta(&app, token, "doc_note", &[]).await;
    note.expect_status(200, "meta for doc_note");
    let kinds: Vec<String> = note.data()["fields"]
        .as_array()
        .must()
        .iter()
        .map(|field| field["type"].as_str().must().to_owned())
        .collect();
    let contract_enum: std::collections::BTreeSet<&str> = [
        "bool",
        "int",
        "decimal",
        "text",
        "long_text",
        "date",
        "time",
        "datetime",
        "link",
        "json",
        "bytes",
        "attach",
        "secret",
        "table",
        "table_multiselect",
    ]
    .into_iter()
    .collect();
    for kind in &kinds {
        assert!(
            contract_enum.contains(kind.as_str()),
            "`{kind}` is not one of the contract's FieldType values"
        );
    }
    for expected in [
        "bool", "int", "decimal", "text", "date", "time", "datetime", "json", "bytes",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "doc_note carries no `{expected}` field: {kinds:?}"
        );
    }

    // A posted DocType marks its non-posted fields read-only (the 0.2.0 meta contract).
    let invoice = harness::get_meta(&app, token, "sales_invoice", &[]).await;
    invoice.expect_status(200, "meta for sales_invoice");
    let total = invoice.data()["fields"]
        .as_array()
        .must()
        .iter()
        .find(|field| field["name"] == json!("total"))
        .must()
        .clone();
    assert_eq!(total["read_only"], json!(true), "posted DocType's field");
    assert_eq!(total["scale"], json!(2));
    assert_eq!(total["precision"], json!(18));
    let items = invoice.data()["fields"]
        .as_array()
        .must()
        .iter()
        .find(|field| field["name"] == json!("items"))
        .must()
        .clone();
    assert_eq!(items["type"], json!("table"));
    assert_eq!(items["child_doctype"], json!("sales_invoice_item"));
    assert!(
        items.get("link_to").is_none(),
        "a table names its rows through child_doctype: {items}"
    );

    // A field with a real override carries its spec, for information (the fixture points
    // the invoice's `customer` at `test:fixed`), and a hinted field carries `hints`.
    let customer = invoice.data()["fields"]
        .as_array()
        .must()
        .iter()
        .find(|field| field["name"] == json!("customer"))
        .must()
        .clone();
    assert_eq!(
        customer["lookup"]["seam"],
        json!(support::fake::SEAM_FIXED),
        "the definition's override seam is for information"
    );
    assert_eq!(customer["lookup"]["version"], json!(1));
    assert_eq!(customer["link_to"], json!("customer"));
    assert_eq!(
        customer["hints"]["in_standard_filter"],
        json!(true),
        "the hint the definition carries"
    );
    assert_eq!(
        customer["hints"].as_object().must().len(),
        1,
        "unset hints are omitted, so exactly one survives"
    );
}
