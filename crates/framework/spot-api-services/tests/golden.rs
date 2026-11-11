//! The golden form-event suite (WP-UI-01.T1).
//!
//! Each case is a request/response pair under `tests/golden/`. The request is the draft and the
//! changed field exactly as a client sends it; the response is the diff the server answered.
//! Desktop and browser replay the same files, so one source of truth fixes both. The suite
//! fails when a file is missing, differs from the case table, or the implementation's answer
//! differs by one byte.
//!
//! Regenerate after a deliberate change with `UPDATE_GOLDEN=1 cargo test -p spot-api-services
//! --test golden`, then read the diff: every changed response is a behaviour change.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod form_support;

use std::path::PathBuf;

use form_support::{
    CUSTOMER_ACME, CUSTOMER_GLOBEX, DOC, Forms, ITEM_GADGET, ITEM_WIDGET, ROW_ONE, ROW_TWO,
};
use serde_json::{Value as Json, json};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// A row of the fixture with every value the client holds.
fn row(id: Option<&str>, item: &str, qty: &str, rate: &str) -> Json {
    let mut row = json!({"item": item, "qty": qty, "rate": rate});
    if let Some(id) = id {
        row["id"] = Json::String(id.to_owned());
    }
    row
}

/// The 30 edits the suite proves.
fn cases() -> Vec<(&'static str, Json)> {
    let mut out: Vec<(&'static str, Json)> = Vec::new();
    let mut push = |name: &'static str, doc: Json, changed: &str, base: Option<u64>| {
        let mut body = json!({"doc": doc, "changed": changed});
        if let Some(base) = base {
            body["base_version"] = Json::from(base);
        }
        out.push((name, body));
    };

    push(
        "001-line-qty-total",
        json!({"number": "INV-1", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "2", "12.50")]}),
        "lines[1].qty",
        None,
    );
    push(
        "002-line-qty-two-rows",
        json!({"number": "INV-2", "lines": [
            row(Some(ROW_ONE), ITEM_WIDGET, "2", "12.50"),
            row(Some(ROW_TWO), ITEM_GADGET, "1", "7.25")
        ]}),
        "lines[2].qty",
        None,
    );
    push(
        "003-line-rate-change",
        json!({"number": "INV-3", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "3", "10.00")]}),
        "lines[1].rate",
        None,
    );
    push(
        "004-line-fractional-qty",
        json!({"number": "INV-4", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "0.333", "3.00")]}),
        "lines[1].qty",
        None,
    );
    push(
        "005-line-zero-qty",
        json!({"number": "INV-5", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "0", "12.50")]}),
        "lines[1].qty",
        None,
    );
    push(
        "006-line-added-row",
        json!({"number": "INV-6", "lines": [
            row(Some(ROW_ONE), ITEM_WIDGET, "1", "12.50"),
            json!({"item": ITEM_GADGET, "qty": "1", "rate": "7.25"})
        ]}),
        "lines",
        None,
    );
    push(
        "007-line-removed-row-totals",
        json!({"number": "INV-7", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "2", "12.50")]}),
        "lines",
        None,
    );
    push(
        "008-line-reordered",
        json!({"number": "INV-8", "lines": [
            row(Some(ROW_TWO), ITEM_GADGET, "1", "7.25"),
            row(Some(ROW_ONE), ITEM_WIDGET, "2", "12.50")
        ]}),
        "lines",
        None,
    );
    push(
        "009-customer-name",
        json!({"number": "INV-9", "customer": CUSTOMER_ACME}),
        "customer",
        None,
    );
    push(
        "010-customer-globex",
        json!({"number": "INV-10", "customer": CUSTOMER_GLOBEX}),
        "customer",
        None,
    );
    push(
        "011-customer-cleared",
        json!({"number": "INV-11", "customer": null}),
        "customer",
        None,
    );
    push(
        "012-customer-echo-empty-patch",
        json!({"number": "INV-12", "customer": CUSTOMER_ACME, "customer_name": "Acme Ltd"}),
        "customer",
        None,
    );
    push(
        "013-item-fetch-first",
        json!({"number": "INV-13", "lines": [row(Some(ROW_ONE), ITEM_GADGET, "2", "12.50")]}),
        "lines[1].item",
        None,
    );
    push(
        "014-item-fetch-second",
        json!({"number": "INV-14", "lines": [
            row(Some(ROW_ONE), ITEM_GADGET, "1", "7.25"),
            row(Some(ROW_TWO), ITEM_WIDGET, "1", "7.25")
        ]}),
        "lines[2].item",
        None,
    );
    push(
        "015-item-unknown",
        json!({"number": "INV-15", "lines": [
            {"id": ROW_ONE, "item": null, "qty": "2", "rate": "12.50"}
        ]}),
        "lines[1].item",
        None,
    );
    push(
        "016-discount-total",
        json!({"number": "INV-16", "discount": "5.00", "lines": [
            row(Some(ROW_ONE), ITEM_WIDGET, "2", "12.50"),
            row(Some(ROW_TWO), ITEM_GADGET, "2", "7.25")
        ]}),
        "discount",
        None,
    );
    push(
        "017-discount-zero",
        json!({"number": "INV-17", "discount": "0", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "1", "1.00")]}),
        "discount",
        None,
    );
    push(
        "018-discount-larger",
        json!({"number": "INV-18", "discount": "20.00", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "1", "12.50")]}),
        "discount",
        None,
    );
    push(
        "019-qty-warning",
        json!({"number": "INV-19", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "50", "1.00")]}),
        "lines[1].qty",
        None,
    );
    push(
        "020-qty-warning-two-rows",
        json!({"number": "INV-20", "lines": [
            row(Some(ROW_ONE), ITEM_WIDGET, "50", "1.00"),
            row(Some(ROW_TWO), ITEM_GADGET, "11", "1.00")
        ]}),
        "lines[2].qty",
        None,
    );
    push(
        "021-number-masked-secret",
        json!({"number": "INV-21", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "1", "1.00")]}),
        "number",
        None,
    );
    push(
        "022-secret-unchanged",
        json!({"number": "INV-22", "secret": "SEC-INV-22", "hidden_note": "audit:INV-22",
               "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "1", "1.00")]}),
        "number",
        None,
    );
    push(
        "023-base-version-stale",
        json!({"id": DOC, "number": "INV-23", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "4", "2.00")]}),
        "lines[1].qty",
        Some(1),
    );
    push(
        "024-base-version-absent",
        json!({"number": "INV-24", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "4", "2.00")]}),
        "lines[1].qty",
        None,
    );
    push(
        "025-draft-hidden-echo",
        json!({"number": "INV-25", "hidden_note": "client text", "secret": "SEC-INV-25",
               "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "2", "3.00")]}),
        "lines[1].qty",
        None,
    );
    push(
        "026-three-line-total",
        json!({"number": "INV-26", "lines": [
            row(Some(ROW_ONE), ITEM_WIDGET, "1", "12.50"),
            row(Some(ROW_TWO), ITEM_GADGET, "2", "7.25"),
            row(None, ITEM_WIDGET, "3", "12.50")
        ]}),
        "lines[3].qty",
        None,
    );
    push(
        "027-line-rate-zero",
        json!({"number": "INV-27", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "5", "0.00")]}),
        "lines[1].rate",
        None,
    );
    push(
        "028-line-qty-precision",
        json!({"number": "INV-28", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "1234.567", "0.01")]}),
        "lines[1].qty",
        None,
    );
    push(
        "029-customer-and-totals",
        json!({"number": "INV-29", "customer": CUSTOMER_ACME,
               "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "2", "12.50")]}),
        "customer",
        None,
    );
    push(
        "030-number-message-filter",
        json!({"id": DOC, "number": "INV-30", "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "1", "1.00")]}),
        "number",
        Some(3),
    );
    out
}

fn pretty(value: &Json) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
}

#[tokio::test]
async fn golden_requests_and_responses_match() {
    let forms = Forms::new().await;
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let dir = golden_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let cases = cases();
    assert_eq!(cases.len(), 30, "the suite is the brief's 30 edits");
    for (name, request) in cases {
        let request_path = dir.join(format!("{name}.request.json"));
        let response_path = dir.join(format!("{name}.response.json"));
        if update {
            std::fs::write(&request_path, pretty(&request)).unwrap();
        }
        let stored_request = std::fs::read_to_string(&request_path)
            .unwrap_or_else(|e| panic!("{}: {e}", request_path.display()));
        assert_eq!(
            stored_request,
            pretty(&request),
            "{name}: the stored request drifted from the case table"
        );

        let answered = forms
            .call(&request)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let response = pretty(&answered.to_json());
        if update {
            std::fs::write(&response_path, &response).unwrap();
        }
        let stored_response = std::fs::read_to_string(&response_path)
            .unwrap_or_else(|e| panic!("{}: {e}", response_path.display()));
        assert_eq!(stored_response, response, "{name}: response drifted");
    }
}

#[tokio::test]
async fn the_same_edit_from_two_protocols_answers_the_same_bytes() {
    let forms = Forms::new().await;
    let request = json!({
        "doc": {
            "number": "INV-P",
            "customer": CUSTOMER_ACME,
            "lines": [row(Some(ROW_ONE), ITEM_WIDGET, "2", "12.50")]
        },
        "changed": "lines[1].qty"
    });
    // The browser builds a compact body; the desktop client pretty-prints it and sends the
    // keys in another order. Both decode to the same draft.
    let compact = serde_json::to_string(&request).unwrap();
    let mut reordered = serde_json::Map::new();
    for key in ["changed", "doc"] {
        reordered.insert(key.to_owned(), request[key].clone());
    }
    let spaced = serde_json::to_string_pretty(&Json::Object(reordered)).unwrap();

    let one = forms.raw(compact.as_bytes()).await.unwrap();
    let two = forms.raw(spaced.as_bytes()).await.unwrap();
    assert_eq!(
        serde_json::to_vec(&one.to_json()).unwrap(),
        serde_json::to_vec(&two.to_json()).unwrap()
    );
}
