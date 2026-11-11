//! The field-lookup endpoint (`POST /api/v1/lookup/{doctype}/{field}`, ADR-068 §3.5):
//! decode and validate the body, call the resolved seam through [`LookupService`], encode
//! the page. The request carries no seam — a body or query key that names one is an unknown
//! property (`api.invalid_request`) — and a failing seam is `lookup.seam_failed` with the
//! explanation, never the default list (ADR-068 §3.8).

use crate::api::lookup::{
    LookupColumn, LookupPage, LookupRequest, LookupRow, MAX_CELL_CHARS, MAX_COLUMNS,
};
use crate::api::registry::Code;
use crate::api::service::ApiError;
use crate::request::RequestId;
use crate::site::SiteContext;
use axum::extract::{Path, Request};
use axum::http::{HeaderMap, StatusCode};
use serde_json::Value as Json;

use super::{
    Failure, HandlerResult, SharedState, build_ctx, fail, json_response, path_identifier,
    take_body, validated_key,
};

/// The body schema's `maxLength` of `text` (contract 0.2.0, `LookupRequest`).
const MAX_TEXT_LEN: usize = 1024;

/// The body keys the contract's `LookupRequest` allows; anything else — in particular a
/// `lookup` object or a `seam` string — is `api.invalid_request` (the schema's
/// `additionalProperties: false`, which keeps a client from naming a seam in any form).
const BODY_KEYS: [&str; 4] = ["text", "cursor", "doc", "parent"];

pub(super) async fn lookup_search(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, field)): Path<(String, String)>,
    request: Request,
) -> HandlerResult {
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let key = validated_key(&request);
    let ctx = build_ctx(&site, &headers, key);
    let body = take_body(request, true, &site.request_id).await?;
    let json = body.unwrap_or_else(|| Json::Object(serde_json::Map::new()));
    let decoded = decode_request(&json, &site.request_id)?;
    let page = state
        .lookup
        .search(&ctx, &doctype, &field, &decoded)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let data = encode_page(&page).map_err(|e| fail(e, &site.request_id))?;
    Ok(json_response(StatusCode::OK, super::envelope(data, &[])))
}

/// Validates the contract's `LookupRequest`: an object with exactly `text` (a string of at
/// most 1024 characters), an optional opaque string `cursor`, and optional object `doc` and
/// `parent` snapshots. The snapshots pass through as JSON in the value encoding; the seam
/// reads them.
fn decode_request(json: &Json, request_id: &RequestId) -> Result<LookupRequest, Failure> {
    let Json::Object(map) = json else {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "The lookup body must be an object.",
            ),
            request_id,
        ));
    };
    for key in map.keys() {
        if !BODY_KEYS.contains(&key.as_str()) {
            return Err(fail(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    format!("`{key}` is not a lookup request field."),
                ),
                request_id,
            ));
        }
    }
    let Some(Json::String(text)) = map.get("text") else {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "`text` is required and must be a string.",
            ),
            request_id,
        ));
    };
    if text.chars().count() > MAX_TEXT_LEN {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                format!("`text` must be at most {MAX_TEXT_LEN} characters."),
            ),
            request_id,
        ));
    }
    let cursor = match map.get("cursor") {
        None | Some(Json::Null) => None,
        Some(Json::String(c)) => Some(c.clone()),
        Some(_) => {
            return Err(fail(
                ApiError::new(Code::ApiInvalidRequest, "`cursor` must be a string."),
                request_id,
            ));
        }
    };
    // The snapshots are objects per the contract's `LookupRequest.doc`/`.parent`; the field
    // values inside are opaque JSON here and validated where their field names are known.
    let snapshots = |which: &str| -> Result<Option<Json>, Failure> {
        match map.get(which) {
            None | Some(Json::Null) => Ok(None),
            Some(object @ Json::Object(_)) => Ok(Some(object.clone())),
            Some(_) => Err(fail(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    format!("`{which}` must be an object."),
                ),
                request_id,
            )),
        }
    };
    let doc = snapshots("doc")?;
    let parent = snapshots("parent")?;
    Ok(LookupRequest {
        text: text.clone(),
        cursor,
        doc,
        parent,
    })
}

/// The contract's `LookupPage` in the value encoding: ids as canonical hyphenated UUIDv7
/// strings, `cursor` present exactly when `has_more` is true (ADR-055).
fn encode_page(page: &LookupPage) -> Result<Json, ApiError> {
    let mut rows = Vec::with_capacity(page.rows.len());
    for row in &page.rows {
        rows.push(encode_row(row)?);
    }
    let mut data = serde_json::Map::new();
    data.insert("rows".to_owned(), Json::Array(rows));
    if !page.columns.is_empty() {
        data.insert("columns".to_owned(), encode_columns(&page.columns)?);
    }
    data.insert("has_more".to_owned(), Json::from(page.has_more));
    match (&page.cursor, page.has_more) {
        (Some(cursor), true) => {
            data.insert("cursor".to_owned(), Json::String(cursor.clone()));
        }
        (Some(_), false) => {
            return Err(ApiError::new(
                Code::HttpInternal,
                "the lookup page carries a cursor but says no more follow.",
            ));
        }
        (None, false) => {}
        (None, true) => {
            return Err(ApiError::new(
                Code::HttpInternal,
                "the lookup page says more follow but carries no cursor.",
            ));
        }
    }
    Ok(Json::Object(data))
}

fn encode_columns(columns: &[LookupColumn]) -> Result<Json, ApiError> {
    if columns.len() > MAX_COLUMNS {
        return Err(ApiError::new(
            Code::HttpInternal,
            "the lookup page names more display columns than the contract allows.",
        ));
    }
    let mut out = Vec::with_capacity(columns.len());
    for column in columns {
        if spot_storage_contract::Name::new(&column.name).is_err() {
            return Err(ApiError::new(
                Code::HttpInternal,
                "a lookup column name is not an identifier.",
            ));
        }
        let mut map = serde_json::Map::new();
        map.insert("name".to_owned(), Json::String(column.name.clone()));
        map.insert("label".to_owned(), Json::String(column.label.clone()));
        out.push(Json::Object(map));
    }
    Ok(Json::Array(out))
}

fn encode_row(row: &LookupRow) -> Result<Json, ApiError> {
    let mut map = serde_json::Map::new();
    map.insert("id".to_owned(), Json::String(row.id.to_string()));
    map.insert("label".to_owned(), Json::String(row.label.clone()));
    if let Some(description) = &row.description {
        map.insert("description".to_owned(), Json::String(description.clone()));
    }
    if !row.extras.is_empty() {
        let mut extras = serde_json::Map::new();
        for (key, value) in &row.extras {
            extras.insert(key.clone(), value.clone());
        }
        map.insert("extras".to_owned(), Json::Object(extras));
    }
    if !row.cells.is_empty() {
        let mut cells = serde_json::Map::new();
        for (key, value) in &row.cells {
            if value.chars().count() > MAX_CELL_CHARS {
                return Err(ApiError::new(
                    Code::HttpInternal,
                    "a lookup cell is longer than the contract allows.",
                ));
            }
            cells.insert(key.clone(), Json::String(value.clone()));
        }
        map.insert("cells".to_owned(), Json::Object(cells));
    }
    Ok(Json::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Must, MustErr};

    fn decode(json: Json) -> Result<LookupRequest, ApiError> {
        // The endpoint-level failure keeps the request id; the tests inspect only the error.
        decode_request(&json, &RequestId::generate()).map_err(|f| *f.error)
    }

    #[test]
    fn the_four_body_keys_decode() {
        let request = decode(serde_json::json!({
            "text": "acme",
            "cursor": "p:2",
            "doc": {"order_type": "export"},
            "parent": {"party": null}
        }))
        .must();
        assert_eq!(request.text, "acme");
        assert_eq!(request.cursor, Some("p:2".to_owned()));
        assert_eq!(
            request.doc,
            Some(serde_json::json!({"order_type": "export"}))
        );
        assert_eq!(request.parent, Some(serde_json::json!({"party": null})));
    }

    #[test]
    fn text_alone_is_required() {
        let request = decode(serde_json::json!({"text": ""})).must();
        assert_eq!(request.text, "");
        assert_eq!(request.cursor, None);
        let err = decode(serde_json::json!({})).must_err();
        assert_eq!(
            err.code,
            super::super::super::service::ErrorCode::Registry(Code::ApiInvalidRequest)
        );
    }

    #[test]
    fn unknown_keys_are_rejected_so_no_seam_reaches_the_service() {
        for body in [
            serde_json::json!({"text": "x", "lookup": {"seam": "billing:customer_search"}}),
            serde_json::json!({"text": "x", "seam": "platform:link_search"}),
        ] {
            let err = decode(body).must_err();
            assert_eq!(
                err.code,
                super::super::super::service::ErrorCode::Registry(Code::ApiInvalidRequest)
            );
        }
    }

    #[test]
    fn text_over_the_contract_cap_is_rejected() {
        let long = "x".repeat(1025);
        let err = decode(serde_json::json!({ "text": long })).must_err();
        assert_eq!(
            err.code,
            super::super::super::service::ErrorCode::Registry(Code::ApiInvalidRequest)
        );
        // Exactly 1024 characters fits (the cap counts characters).
        let ok = "y".repeat(1024);
        decode(serde_json::json!({ "text": ok })).must();
    }

    #[test]
    fn cursor_and_snapshots_are_typed() {
        let err = decode(serde_json::json!({"text": "x", "cursor": 3})).must_err();
        assert_eq!(
            err.code,
            super::super::super::service::ErrorCode::Registry(Code::ApiInvalidRequest)
        );
        let err = decode(serde_json::json!({"text": "x", "doc": "no"})).must_err();
        assert_eq!(
            err.code,
            super::super::super::service::ErrorCode::Registry(Code::ApiInvalidRequest)
        );
        let err = decode(serde_json::json!({"text": "x", "parent": []})).must_err();
        assert_eq!(
            err.code,
            super::super::super::service::ErrorCode::Registry(Code::ApiInvalidRequest)
        );
    }

    #[test]
    fn a_page_encodes_ids_as_canonical_strings_and_keeps_extras() {
        let page = LookupPage {
            rows: vec![LookupRow {
                id: crate::api::codec::parse_record_id("0192f3c4-0000-7000-8000-00000000c009")
                    .must(),
                label: "Acme Trading Co.".to_owned(),
                description: Some("Karachi, net 30".to_owned()),
                extras: [("currency".to_owned(), serde_json::json!("USD"))]
                    .into_iter()
                    .collect(),
                cells: Default::default(),
            }],
            has_more: false,
            cursor: None,
            columns: Vec::new(),
        };
        let data = encode_page(&page).must();
        assert_eq!(
            data["rows"][0]["id"],
            serde_json::json!("0192f3c4-0000-7000-8000-00000000c009")
        );
        assert_eq!(data["rows"][0]["label"], "Acme Trading Co.");
        assert_eq!(
            data["rows"][0]["extras"]["currency"],
            serde_json::json!("USD")
        );
        assert_eq!(data["has_more"], serde_json::json!(false));
        assert!(data.get("cursor").is_none());
    }

    #[test]
    fn cursor_is_present_exactly_when_has_more() {
        let base = LookupPage {
            rows: Vec::new(),
            has_more: true,
            cursor: None,
            columns: Vec::new(),
        };
        assert!(encode_page(&base).is_err(), "no cursor with has_more");
        let page = LookupPage {
            has_more: false,
            cursor: Some("p:1".to_owned()),
            ..base
        };
        assert!(encode_page(&page).is_err(), "cursor without has_more");
    }

    #[test]
    fn columns_and_cells_encode_and_are_bounded() {
        let column = |name: &str| LookupColumn {
            name: name.to_owned(),
            label: "L".to_owned(),
        };
        let row = |cell: String| LookupRow {
            id: crate::api::codec::parse_record_id("0192f3c4-0000-7000-8000-00000000c009").must(),
            label: "x".to_owned(),
            description: None,
            extras: Default::default(),
            cells: [("city".to_owned(), cell)].into_iter().collect(),
        };
        let page = |columns: Vec<LookupColumn>, cell: String| LookupPage {
            rows: vec![row(cell)],
            has_more: false,
            cursor: None,
            columns,
        };
        let ok = encode_page(&page(vec![column("city")], "Karachi".into())).must();
        assert_eq!(ok["columns"][0]["name"], "city");
        assert_eq!(ok["rows"][0]["cells"]["city"], "Karachi");
        let nine: Vec<_> = (0..9).map(|i| column(&format!("c{i}"))).collect();
        assert!(encode_page(&page(nine, "a".into())).is_err());
        assert!(encode_page(&page(vec![column("Bad Name")], "a".into())).is_err());
        assert!(encode_page(&page(vec![column("city")], "x".repeat(1025))).is_err());
        assert!(encode_page(&page(vec![column("city")], "x".repeat(1024))).is_ok());
    }
}
