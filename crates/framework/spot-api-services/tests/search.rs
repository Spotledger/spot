//! Search through the resource query (WP-STO-13.T5): the service over a scripted store, a field
//! access fake and a projection fake. The permission filter is the store's (the host's guarded
//! engine in production); the fake store applies a per-caller visible set so the tests can prove
//! that rows, facet counts, snippets, totals and cursors never leave the caller's own set.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_api_services::search::{
    ProjectionStatus, SearchAccess, SearchProjection, SearchService, SearchStore, highlight_terms,
    make_snippet,
};
use spot_app_api::context::{Actor, Invocation};
use spot_core::{ManualClock, SiteId};
use spot_http::api::codec::{ApiDocument, FieldValue};
use spot_http::api::meta::{DocTypeMeta, FieldType as ApiFieldType, MetaField};
use spot_http::api::ports::search::{SearchExtensionIn, SearchModeIn, SearchPort, SnippetsIn};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, Credential, ErrorCode, QuerySpec, RequestCtx};
use spot_http::request::RequestId;
use spot_storage_contract::{
    Catalog, Collation, EntityDef, EntityKind, Expr, FieldDef, FieldType, Name, Query, RecordId,
    Rows, SearchDef, SearchField, SearchWeight, Value,
};

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn id(k: u8) -> RecordId {
    spot_http::api::codec::parse_record_id(&format!("01970000-0000-7000-8000-0000000000{k:02x}"))
        .unwrap()
}

fn alice() -> RecordId {
    id(0xa1)
}

fn bob() -> RecordId {
    id(0xb1)
}

const FIELDS: [&str; 4] = ["title", "body", "city", "secret"];

fn catalog(search: bool) -> Arc<Catalog> {
    let text = |name: &str| FieldDef {
        name: n(name),
        ty: FieldType::Text { max_chars: None },
        nullable: true,
    };
    let entity = EntityDef {
        name: n("listing"),
        kind: EntityKind::Document,
        fields: FIELDS.iter().map(|f| text(f)).collect(),
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: search.then(|| SearchDef {
            language: None,
            fields: vec![
                SearchField {
                    name: n("title"),
                    weight: SearchWeight::A,
                },
                SearchField {
                    name: n("body"),
                    weight: SearchWeight::C,
                },
            ],
            facets: vec![n("city")],
            typo_tolerance: false,
        }),
    };
    Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![entity]).unwrap())
}

fn meta() -> DocTypeMeta {
    let field = |name: &str| MetaField {
        name: n(name),
        label: name.to_owned(),
        field_type: ApiFieldType::Text,
        required: false,
        read_only: false,
        scale: None,
        precision: None,
        link_to: None,
        link_type_field: None,
        child_doctype: None,
        link_field: None,
        select: None,
        max_chars: None,
        default: None,
        format: None,
        format_params: Default::default(),
        lookup: None,
        hints: Default::default(),
        has_hints: false,
        multiple: false,
    };
    DocTypeMeta {
        doctype: n("listing"),
        label: "Listing".to_owned(),
        version: 1,
        is_posted: false,
        fields: FIELDS.iter().map(|f| field(f)).collect(),
    }
}

/// The documents: id, title, body, city.
fn docs() -> Vec<(RecordId, &'static str, &'static str, &'static str)> {
    vec![
        (
            id(1),
            "Harbour view",
            "Quiet flat near the <script>alert(1)</script> harbour",
            "Lahore",
        ),
        (id(2), "Garden flat", "Harbour side garden", "Karachi"),
        (id(3), "Old town house", "Near the harbour", "Lahore"),
        (id(4), "Hill cabin", "A harbour of calm", "Murree"),
        (id(5), "City loft", "Harbour lights", "Lahore"),
    ]
}

#[derive(Default)]
struct Seen {
    queries: Vec<Query>,
}

struct Store {
    catalog: Arc<Catalog>,
    seen: Mutex<Seen>,
}

impl Store {
    fn visible(actor: &Actor) -> Vec<RecordId> {
        match actor {
            Actor::User { id, .. } if *id == alice() => (1..=5).map(self::id).collect(),
            _ => vec![id(1), id(3), id(5)],
        }
    }
}

impl SearchStore for Store {
    fn catalog(&self) -> Result<Arc<Catalog>, ApiError> {
        Ok(self.catalog.clone())
    }

    fn doctype_meta<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<DocTypeMeta, ApiError>> {
        Box::pin(async { Ok(meta()) })
    }

    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        _: Arc<Catalog>,
        query: &'a Query,
    ) -> BoxFuture<'a, Result<Rows, ApiError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().queries.push(query.clone());
            let visible = Store::visible(&inv.actor);
            let rows: Vec<_> = docs()
                .into_iter()
                .filter(|d| visible.contains(&d.0))
                .collect();
            if !query.group_by.is_empty() {
                let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
                for d in &rows {
                    *counts.entry(d.3).or_default() += 1;
                }
                let mut out: Vec<_> = counts.into_iter().collect();
                out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                return Ok(Rows {
                    columns: vec![n("v"), n("n")],
                    rows: out
                        .into_iter()
                        .map(|(v, c)| vec![Value::Text(v.to_owned()), Value::Int(c)])
                        .collect(),
                });
            }
            if query.select.len() == 1 && query.select[0].alias.as_str() == "count" {
                return Ok(Rows {
                    columns: vec![n("count")],
                    rows: vec![vec![Value::Int(rows.len() as i64)]],
                });
            }
            let page: Vec<Vec<Value>> = rows
                .iter()
                .skip(query.offset as usize)
                .take(query.limit.unwrap_or(u32::MAX) as usize)
                .map(|d| vec![Value::Id(d.0)])
                .collect();
            Ok(Rows {
                columns: vec![n("id")],
                rows: page,
            })
        })
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        _: &'a str,
        record: RecordId,
    ) -> BoxFuture<'a, Result<ApiDocument, ApiError>> {
        Box::pin(async move {
            if !Store::visible(&inv.actor).contains(&record) {
                return Err(ApiError::from_code(Code::DocumentNotFound));
            }
            let (_, title, body, city) = docs().into_iter().find(|d| d.0 == record).unwrap();
            let mut doc = ApiDocument::empty();
            doc.id = Some(record);
            for (k, v) in [
                ("title", title),
                ("body", body),
                ("city", city),
                ("secret", "hunter2"),
            ] {
                doc.fields
                    .insert(n(k), FieldValue::Scalar(Value::Text(v.to_owned())));
            }
            Ok(doc)
        })
    }
}

/// `secret` is protected; a name that is not a field is refused the same way.
struct Access;

impl SearchAccess for Access {
    fn check<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        fields: &'a [String],
    ) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move {
            if fields
                .iter()
                .all(|f| FIELDS.contains(&f.as_str()) && f != "secret")
            {
                Ok(())
            } else {
                Err(ApiError::from_code(Code::PermissionFieldDenied))
            }
        })
    }

    fn usable<'a>(
        &'a self,
        _: &'a Invocation,
        _: &'a str,
        fields: &'a [String],
    ) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            fields
                .iter()
                .filter(|f| FIELDS.contains(&f.as_str()) && *f != "secret")
                .cloned()
                .collect()
        })
    }
}

struct Projection(ProjectionStatus);

impl SearchProjection for Projection {
    fn status<'a>(&'a self, _: &'a Invocation, _: &'a str) -> BoxFuture<'a, ProjectionStatus> {
        let status = self.0;
        Box::pin(async move { status })
    }
}

struct Fixture {
    service: SearchService,
    store: Arc<Store>,
}

fn fixture_with(search: bool, status: Option<ProjectionStatus>) -> Fixture {
    let store = Arc::new(Store {
        catalog: catalog(search),
        seen: Mutex::new(Seen::default()),
    });
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
    ));
    let mut service = SearchService::new(
        store.clone(),
        Arc::new(Access),
        clock,
        Arc::new(|ctx: &RequestCtx| {
            let token = ctx.credential.as_ref().map(|c| c.token.as_str());
            match token {
                Some("alice") => Ok(Actor::User {
                    id: alice(),
                    via: None,
                }),
                Some("bob") => Ok(Actor::User {
                    id: bob(),
                    via: None,
                }),
                _ => Err(ApiError::from_code(Code::AuthRequired)),
            }
        }),
    );
    if let Some(status) = status {
        service = service.with_projection(Arc::new(Projection(status)));
    }
    Fixture { service, store }
}

fn fixture() -> Fixture {
    fixture_with(
        true,
        Some(ProjectionStatus {
            available: true,
            lag: None,
            bound: None,
        }),
    )
}

fn ctx(token: &str) -> RequestCtx {
    RequestCtx {
        site: SiteId::new("acme").unwrap(),
        credential: Some(Credential::new(token)),
        idempotency_key: None,
        language: None,
        request_id: RequestId::new("req-1").unwrap(),
        peer: None,
    }
}

fn spec(limit: u32, cursor: Option<String>) -> QuerySpec {
    QuerySpec {
        fields: vec![],
        filter: None,
        order: vec![],
        limit,
        cursor,
        with_total: false,
    }
}

fn search(text: &str) -> SearchExtensionIn {
    SearchExtensionIn {
        mode: SearchModeIn::Web,
        text: text.to_owned(),
        fields: vec![],
        rank: true,
        facets: vec![],
        snippets: None,
    }
}

fn code_of(error: &ApiError) -> String {
    match &error.code {
        ErrorCode::Registry(code) => format!("{code:?}"),
        ErrorCode::App(text) => text.clone(),
    }
}

fn ids(out: &spot_http::api::ports::search::SearchPageOut) -> Vec<RecordId> {
    out.page.documents.iter().map(|d| d.id.unwrap()).collect()
}

#[tokio::test]
async fn a_ranked_search_pages_by_a_stable_cursor_and_ranks_first() {
    let f = fixture();
    let ext = search("harbour");
    let mut seen = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let s = spec(2, cursor.clone());
        let out = f
            .service
            .query(&ctx("alice"), "listing", &s, &ext)
            .await
            .unwrap();
        seen.extend(ids(&out));
        pages += 1;
        cursor = out.page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(pages, 3, "three pages of 2, 2 and 1");
    assert_eq!(
        seen,
        (1..=5).map(id).collect::<Vec<_>>(),
        "every row once, in order"
    );
    let queries = f.store.seen.lock().unwrap().queries.clone();
    let page_query = queries.first().unwrap();
    assert!(
        matches!(page_query.order_by[0].expr, Expr::SearchRank(_)),
        "rank is the first key"
    );
    assert!(
        matches!(page_query.order_by.last().unwrap().expr, Expr::Field(_)),
        "the id breaks ties"
    );
    assert!(
        matches!(&page_query.filter, Some(Expr::Search(_))),
        "the term is the filter"
    );
}

#[tokio::test]
async fn a_search_never_returns_a_row_the_caller_may_not_read_and_totals_follow() {
    let f = fixture();
    let mut s = spec(10, None);
    s.with_total = true;
    let bob_page = f
        .service
        .query(&ctx("bob"), "listing", &s, &search("harbour"))
        .await
        .unwrap();
    assert_eq!(ids(&bob_page), vec![id(1), id(3), id(5)]);
    assert_eq!(
        bob_page.page.total,
        Some(3),
        "the total counts the caller's rows only"
    );
    let alice_page = f
        .service
        .query(&ctx("alice"), "listing", &s, &search("harbour"))
        .await
        .unwrap();
    assert_eq!(alice_page.page.total, Some(5));
}

#[tokio::test]
async fn facet_counts_are_over_the_callers_visible_set() {
    let f = fixture();
    let mut ext = search("harbour");
    ext.facets = vec!["city".to_owned()];
    let a = f
        .service
        .query(&ctx("alice"), "listing", &spec(10, None), &ext)
        .await
        .unwrap();
    let b = f
        .service
        .query(&ctx("bob"), "listing", &spec(10, None), &ext)
        .await
        .unwrap();
    let count = |out: &spot_http::api::ports::search::SearchPageOut, city: &str| {
        out.facets.as_ref().unwrap()["city"]
            .iter()
            .find(|v| v.value.as_deref() == Some(city))
            .map_or(0, |v| v.count)
    };
    assert_eq!(count(&a, "Lahore"), 3);
    assert_eq!(count(&a, "Karachi"), 1);
    assert_eq!(count(&b, "Lahore"), 3);
    assert_eq!(
        count(&b, "Karachi"),
        0,
        "bob cannot see the Karachi row, so it is not counted"
    );
    assert_eq!(count(&b, "Murree"), 0);
    let none = f
        .service
        .query(
            &ctx("alice"),
            "listing",
            &spec(10, None),
            &search("harbour"),
        )
        .await
        .unwrap();
    assert!(none.facets.is_none(), "no facets unless asked");
}

#[tokio::test]
async fn a_protected_or_unknown_field_is_field_denied_in_every_channel() {
    let f = fixture();
    let mut in_fields = search("harbour");
    in_fields.fields = vec!["secret".to_owned()];
    let mut in_facets = search("harbour");
    in_facets.facets = vec!["secret".to_owned()];
    let mut in_snippets = search("harbour");
    in_snippets.snippets = Some(SnippetsIn {
        fields: vec!["secret".to_owned()],
        max_chars: 80,
    });
    let mut unknown = search("harbour");
    unknown.fields = vec!["no_such_field".to_owned()];
    for (what, ext) in [
        ("fields", in_fields),
        ("facets", in_facets),
        ("snippets", in_snippets),
        ("unknown", unknown),
    ] {
        let error = f
            .service
            .query(&ctx("alice"), "listing", &spec(10, None), &ext)
            .await
            .unwrap_err();
        assert_eq!(code_of(&error), "PermissionFieldDenied", "{what}");
    }
    // Storage was not touched by any of them.
    assert!(f.store.seen.lock().unwrap().queries.is_empty());
}

#[tokio::test]
async fn a_readable_field_that_is_not_declared_is_not_searchable() {
    let f = fixture();
    let mut by_city = search("harbour");
    by_city.fields = vec!["city".to_owned()];
    let error = f
        .service
        .query(&ctx("alice"), "listing", &spec(10, None), &by_city)
        .await
        .unwrap_err();
    assert_eq!(code_of(&error), "SearchFieldNotSearchable");
    let mut facet_title = search("harbour");
    facet_title.facets = vec!["title".to_owned()];
    let error = f
        .service
        .query(&ctx("alice"), "listing", &spec(10, None), &facet_title)
        .await
        .unwrap_err();
    assert_eq!(code_of(&error), "SearchFieldNotSearchable");
}

#[tokio::test]
async fn snippets_are_escaped_marked_and_only_for_the_page_rows_the_caller_sees() {
    let f = fixture();
    let mut ext = search("harbour");
    ext.snippets = Some(SnippetsIn {
        fields: vec!["body".to_owned()],
        max_chars: 200,
    });
    let out = f
        .service
        .query(&ctx("bob"), "listing", &spec(10, None), &ext)
        .await
        .unwrap();
    let snippets = out.snippets.unwrap();
    assert_eq!(snippets.len(), 3, "bob's three rows only");
    assert!(!snippets.contains_key(&id(2).to_string()));
    let first = &snippets[&id(1).to_string()]["body"];
    assert!(first.contains("&lt;script&gt;"), "{first}");
    assert!(!first.contains("<script"), "{first}");
    assert!(first.contains("<mark>harbour</mark>"), "{first}");
    assert!(!first.contains("hunter2"));
}

#[tokio::test]
async fn an_unsupported_backend_a_missing_declaration_and_a_lagging_projection_answer_their_codes()
{
    // No projection installed: the in-memory fake's case.
    let none = fixture_with(true, None);
    let error = none
        .service
        .query(
            &ctx("alice"),
            "listing",
            &spec(10, None),
            &search("harbour"),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&error), "SearchUnsupported");
    // The DocType declares no search.
    let undeclared = fixture_with(
        false,
        Some(ProjectionStatus {
            available: true,
            lag: None,
            bound: None,
        }),
    );
    let error = undeclared
        .service
        .query(
            &ctx("alice"),
            "listing",
            &spec(10, None),
            &search("harbour"),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&error), "SearchUnsupported");
    // The backend says it has no search.
    let off = fixture_with(
        true,
        Some(ProjectionStatus {
            available: false,
            lag: None,
            bound: None,
        }),
    );
    let error = off
        .service
        .query(
            &ctx("alice"),
            "listing",
            &spec(10, None),
            &search("harbour"),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&error), "SearchUnsupported");
    // Lag past the bound refuses, lag within it is reported.
    let behind = fixture_with(
        true,
        Some(ProjectionStatus {
            available: true,
            lag: Some(900),
            bound: Some(500),
        }),
    );
    let error = behind
        .service
        .query(
            &ctx("alice"),
            "listing",
            &spec(10, None),
            &search("harbour"),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&error), "SearchProjectionBehind");
    let near = fixture_with(
        true,
        Some(ProjectionStatus {
            available: true,
            lag: Some(40),
            bound: Some(500),
        }),
    );
    let out = near
        .service
        .query(
            &ctx("alice"),
            "listing",
            &spec(10, None),
            &search("harbour"),
        )
        .await
        .unwrap();
    assert_eq!(out.projection_lag, Some(40));
}

#[tokio::test]
async fn a_cursor_of_another_search_or_caller_is_refused() {
    let f = fixture();
    let first = f
        .service
        .query(&ctx("alice"), "listing", &spec(2, None), &search("harbour"))
        .await
        .unwrap();
    let cursor = first.page.next_cursor.unwrap();
    let other_text = f
        .service
        .query(
            &ctx("alice"),
            "listing",
            &spec(2, Some(cursor.clone())),
            &search("garden"),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&other_text), "ApiInvalidRequest");
    let other_caller = f
        .service
        .query(
            &ctx("bob"),
            "listing",
            &spec(2, Some(cursor)),
            &search("harbour"),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&other_caller), "ApiInvalidRequest");
    let garbage = f
        .service
        .query(
            &ctx("alice"),
            "listing",
            &spec(2, Some("not-a-cursor".to_owned())),
            &search("harbour"),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&garbage), "ApiInvalidRequest");
}

#[tokio::test]
async fn describe_lists_only_what_the_caller_may_use() {
    let f = fixture();
    let out = f.service.describe(&ctx("alice"), "listing").await.unwrap();
    assert_eq!(out.searchable, vec!["title".to_owned(), "body".to_owned()]);
    assert_eq!(out.facetable, vec!["city".to_owned()]);
    let undeclared = fixture_with(false, None);
    let out = undeclared
        .service
        .describe(&ctx("alice"), "listing")
        .await
        .unwrap();
    assert!(out.searchable.is_empty() && out.facetable.is_empty());
}

#[test]
fn a_snippet_is_escaped_bounded_and_marks_only_the_matches() {
    let terms = highlight_terms("Harbour -garden \"quiet flat\"", SearchModeIn::Web);
    assert!(terms.contains(&"harbour".chars().collect::<Vec<_>>()));
    assert!(
        !terms.contains(&"garden".chars().collect::<Vec<_>>()),
        "an excluded word is not marked"
    );
    let text = format!(
        "{} Harbour & <b>co</b> {}",
        "x".repeat(300),
        "y".repeat(300)
    );
    let snippet = make_snippet(&text, &terms, 60);
    assert!(snippet.contains("<mark>Harbour</mark>"), "{snippet}");
    assert!(
        snippet.contains("&amp;") && snippet.contains("&lt;b&gt;"),
        "{snippet}"
    );
    assert!(!snippet.contains("<b>"), "{snippet}");
    assert!(
        snippet.chars().count() < 120,
        "bounded: {}",
        snippet.chars().count()
    );
    // No match: the beginning of the text.
    let plain = make_snippet("nothing to see here", &terms, 10);
    assert!(plain.starts_with("nothing to"), "{plain}");
    assert!(!plain.contains("<mark>"));
    // Whatever the input, the answer is bounded by the contract's 1,000 characters.
    let hostile = "&".repeat(5_000);
    let all = highlight_terms("&", SearchModeIn::Plain);
    assert!(make_snippet(&hostile, &all, 200).chars().count() <= 1_000);
}
