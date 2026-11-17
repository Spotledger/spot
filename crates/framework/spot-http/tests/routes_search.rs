//! The search members of the resource query (WP-STO-13.T5): the shape checks of
//! `api::router::routes::search::take_extension`, the response members and the scripted port.
//! The service half (permissions, facets, snippets, cursors) is tested in
//! `spot-api-services/tests/search.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use spot_http::api::ports::search::{
    FacetValueOut, MemorySearchPort, SearchModeIn, SearchPageOut, SearchPort, SearchableOut,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::search::{meta_members, page_members, take_extension};
use spot_http::api::service::{ApiError, ErrorCode, Page};

fn code(error: &ApiError) -> Code {
    match &error.code {
        ErrorCode::Registry(code) => *code,
        ErrorCode::App(text) => panic!("unexpected app code {text}"),
    }
}

fn take(
    mut body: Json,
) -> (
    Result<Option<spot_http::api::ports::search::SearchExtensionIn>, ApiError>,
    Json,
) {
    let out = take_extension(&mut body);
    (out, body)
}

#[test]
fn a_query_without_search_members_is_left_alone() {
    let body = json!({"filter": {"field": "name", "op": "eq", "value": "x"}, "limit": 5});
    let (out, left) = take(body.clone());
    assert_eq!(out.unwrap(), None);
    assert_eq!(left, body);
    let (out, left) = take(json!({"search": null, "limit": 5}));
    assert_eq!(out.unwrap(), None);
    assert_eq!(
        left,
        json!({"limit": 5}),
        "an explicit null search is dropped"
    );
}

#[test]
fn a_search_is_taken_out_with_its_members_and_a_leading_rank_key() {
    let (out, left) = take(json!({
        "search": {"text": "harbour", "mode": "phrase", "fields": ["title", "body"]},
        "facets": ["city"],
        "snippets": {"fields": ["body"], "max_chars": 80},
        "order": [{"rank": true}, {"field": "title", "dir": "asc"}],
        "limit": 10
    }));
    let ext = out.unwrap().unwrap();
    assert_eq!(ext.mode, SearchModeIn::Phrase);
    assert_eq!(ext.text, "harbour");
    assert_eq!(ext.fields, vec!["title", "body"]);
    assert!(ext.rank);
    assert_eq!(ext.facets, vec!["city"]);
    assert_eq!(ext.snippets.unwrap().max_chars, 80);
    assert_eq!(
        left,
        json!({"order": [{"field": "title", "dir": "asc"}], "limit": 10})
    );
    // Rank alone leaves no order at all; defaults apply.
    let (out, left) = take(json!({"search": {"text": "x"}, "order": [{"rank": true}]}));
    let ext = out.unwrap().unwrap();
    assert!(ext.rank && ext.mode == SearchModeIn::Web && ext.fields.is_empty());
    assert_eq!(left, json!({}));
    let (out, _) = take(json!({"search": {"text": "x"}, "snippets": {"fields": ["a"]}}));
    assert_eq!(out.unwrap().unwrap().snippets.unwrap().max_chars, 120);
}

#[test]
fn every_malformed_search_member_is_refused_with_the_contracts_code() {
    let bad: Vec<(&str, Json, Code)> = vec![
        (
            "facets without search",
            json!({"facets": ["city"]}),
            Code::ApiInvalidRequest,
        ),
        (
            "snippets without search",
            json!({"snippets": {"fields": ["a"]}}),
            Code::ApiInvalidRequest,
        ),
        (
            "rank without search",
            json!({"order": [{"rank": true}]}),
            Code::ApiInvalidRequest,
        ),
        (
            "rank not first",
            json!({"search": {"text": "x"}, "order": [{"field": "a"}, {"rank": true}]}),
            Code::ApiInvalidRequest,
        ),
        (
            "rank false",
            json!({"search": {"text": "x"}, "order": [{"rank": false}]}),
            Code::ApiInvalidRequest,
        ),
        (
            "rank with field",
            json!({"search": {"text": "x"}, "order": [{"rank": true, "field": "a"}]}),
            Code::ApiInvalidRequest,
        ),
        (
            "text over 512",
            json!({"search": {"text": "x".repeat(513)}}),
            Code::SearchTermTooLong,
        ),
        (
            "blank text",
            json!({"search": {"text": "   "}}),
            Code::ApiInvalidRequest,
        ),
        ("no text", json!({"search": {}}), Code::ApiInvalidRequest),
        (
            "bad mode",
            json!({"search": {"text": "x", "mode": "fuzzy"}}),
            Code::ApiInvalidRequest,
        ),
        (
            "unknown member",
            json!({"search": {"text": "x", "typo": true}}),
            Code::ApiInvalidRequest,
        ),
        (
            "no fields",
            json!({"search": {"text": "x", "fields": []}}),
            Code::ApiInvalidRequest,
        ),
        (
            "17 fields",
            json!({"search": {"text": "x", "fields": (0..17).map(|i| format!("f{i}")).collect::<Vec<_>>()}}),
            Code::ApiInvalidRequest,
        ),
        (
            "duplicate field",
            json!({"search": {"text": "x", "fields": ["a", "a"]}}),
            Code::ApiInvalidRequest,
        ),
        (
            "bad field name",
            json!({"search": {"text": "x", "fields": ["No Good"]}}),
            Code::ApiInvalidRequest,
        ),
        (
            "5 facets",
            json!({"search": {"text": "x"}, "facets": ["a", "b", "c", "d", "e"]}),
            Code::ApiInvalidRequest,
        ),
        (
            "snippet too short",
            json!({"search": {"text": "x"}, "snippets": {"fields": ["a"], "max_chars": 19}}),
            Code::ApiInvalidRequest,
        ),
        (
            "snippet too long",
            json!({"search": {"text": "x"}, "snippets": {"fields": ["a"], "max_chars": 201}}),
            Code::ApiInvalidRequest,
        ),
        (
            "snippets without fields",
            json!({"search": {"text": "x"}, "snippets": {}}),
            Code::ApiInvalidRequest,
        ),
    ];
    for (what, body, expected) in bad {
        let (out, _) = take(body);
        let error = out.unwrap_err();
        assert_eq!(code(&error), expected, "{what}");
        // The text and names never come back in the message.
        assert!(!error.message.contains("xxxx"), "{what}");
    }
    // 512 characters exactly is allowed (counted in characters, not bytes).
    let (out, _) = take(json!({"search": {"text": "\u{0627}".repeat(512)}}));
    assert!(out.unwrap().is_some());
}

#[test]
fn the_page_and_meta_members_follow_the_contract() {
    let mut facets = BTreeMap::new();
    facets.insert(
        "city".to_owned(),
        vec![
            FacetValueOut {
                value: Some("Lahore".into()),
                count: 3,
            },
            FacetValueOut {
                value: None,
                count: 1,
            },
        ],
    );
    let mut by_field = BTreeMap::new();
    by_field.insert(
        "body".to_owned(),
        "near the <mark>harbour</mark>".to_owned(),
    );
    let mut snippets = BTreeMap::new();
    snippets.insert("0192f3c4-0000-7000-8000-00000000a001".to_owned(), by_field);
    let out = SearchPageOut {
        page: Page {
            documents: vec![],
            next_cursor: None,
            total: None,
        },
        facets: Some(facets),
        snippets: Some(snippets),
        projection_lag: Some(7),
    };
    let members = page_members(&out);
    assert_eq!(
        Json::Object(members),
        json!({
            "facets": {"city": [{"value": "Lahore", "count": 3}, {"value": null, "count": 1}]},
            "snippets": {"0192f3c4-0000-7000-8000-00000000a001": {"body": "near the <mark>harbour</mark>"}},
            "projection_lag": 7
        })
    );
    let plain = SearchPageOut {
        facets: None,
        snippets: None,
        projection_lag: None,
        ..out
    };
    assert!(
        page_members(&plain).is_empty(),
        "nothing is added unless present"
    );
    let meta = meta_members(&SearchableOut {
        searchable: vec!["title".into()],
        facetable: vec![],
    });
    assert_eq!(Json::Object(meta), json!({"searchable": ["title"]}));
}

#[tokio::test]
async fn the_scripted_port_records_the_extension_and_answers_what_it_is_told() {
    use spot_core::SiteId;
    use spot_http::api::service::{Credential, QuerySpec, RequestCtx};
    use spot_http::request::RequestId;
    let port = MemorySearchPort::new();
    let ctx = RequestCtx {
        site: SiteId::new("acme").unwrap(),
        credential: Some(Credential::new("t")),
        idempotency_key: None,
        language: None,
        request_id: RequestId::new("r").unwrap(),
        peer: None,
    };
    let spec = QuerySpec {
        fields: vec![],
        filter: None,
        order: vec![],
        limit: 5,
        cursor: None,
        with_total: true,
    };
    let (ext, _) = take(json!({"search": {"text": "harbour"}, "facets": ["city"]}));
    let ext = ext.unwrap().unwrap();
    let out = port.query(&ctx, "listing", &spec, &ext).await.unwrap();
    assert_eq!(out.page.total, Some(0));
    assert!(out.facets.is_some() && out.snippets.is_none());
    assert_eq!(port.calls().len(), 1);
    port.answer(Err(ApiError::from_code(Code::SearchUnsupported)));
    let error = port.query(&ctx, "listing", &spec, &ext).await.unwrap_err();
    assert_eq!(code(&error), Code::SearchUnsupported);
}
