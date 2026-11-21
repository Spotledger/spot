//! The list path under the caller's read rights (WP-SEC-04.T15): the row set, `total` and the
//! cursor of `EngineResources::list` over the real document `Engine` with the
//! `PermissionAuthorizer` contain only the rows the caller may read, and carry no trace of the
//! rest (count, cursor, error text).
//!
//! The world is three callers over six notes. Three notes are in the region the scoped caller
//! may use, three are not; the titles of the three it may not see are planted markers that must
//! never appear in anything that caller receives.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_api_services::{EngineResources, query::require_readable};
use spot_app_api::Document;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_core::ManualClock;
use spot_document::engine::{Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_http::api::filter::{Comparison, FilterAst, FilterValue, Op, OrderKey};
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, Credential, ErrorCode, Page, QuerySpec, RequestCtx, ResourceService,
};
use spot_http::request::RequestId;
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::{
    AppliesTo, AssignmentContext, ModelTexts, PermissionEngine, Principal, Role, Snapshot,
    UserPermission, load,
};
use spot_seams::RegistryBuilder;
use spot_storage_contract::OrderDir;
use spot_storage_fake::FakeBackend;

const ALLOWED_REGION: &str = "01890000-0000-7000-8000-000000000001";
const OTHER_REGION: &str = "01890000-0000-7000-8000-000000000002";

/// Visible to the scoped caller, in title order.
const VISIBLE: [&str; 3] = ["a-note-1", "c-note-2", "e-note-3"];
/// Not visible to the scoped caller; each sorts between two visible ones.
const HIDDEN: [&str; 3] = ["b-secret-1", "d-secret-2", "f-secret-3"];

const CALLERS: [&str; 3] = ["full", "scoped", "nobody"];

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn definitions() -> Vec<Definition> {
    let mut region = Definition::new("region");
    region.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut note = Definition::new("note");
    let mut link = FieldSpec::new("region", FieldType::Link);
    link.options = Some("region".into());
    note.fields = vec![FieldSpec::new("title", FieldType::Text), link];
    vec![region, note]
}

fn decision_engine() -> Arc<PermissionEngine> {
    let meta = Registry::new(definitions()).unwrap().effective();
    let model = load(&ModelTexts {
        grants: r#"
[[entries]]
doctype = "region"
role = "reader"
rights = ["read"]

[[entries]]
doctype = "note"
role = "reader"
rights = ["read"]
"#
        .to_owned(),
        field_grants: String::new(),
        groups: String::new(),
        shares: String::new(),
    })
    .unwrap();
    Arc::new(PermissionEngine::new(Arc::new(
        Snapshot::build(&meta, &model).unwrap(),
    )))
}

fn principal(who: &str) -> Principal {
    match who {
        "full" => Principal::with_roles("full", &["reader"]),
        "scoped" => Principal::user(
            "scoped",
            vec![AssignmentContext {
                id: "default".to_owned(),
                roles: vec![Role("reader".to_owned())],
                scope: vec![UserPermission {
                    user: "scoped".to_owned(),
                    doctype: None,
                    field_or_link_doctype: "note.region".to_owned(),
                    allowed: vec![ALLOWED_REGION.to_owned()],
                    applies_to: AppliesTo::AllDoctypes,
                    include_descendants: false,
                }],
            }],
        ),
        _ => Principal::user("nobody", Vec::new()),
    }
}

struct Rig {
    engine: Arc<Engine<FakeBackend>>,
    resources: EngineResources<Engine<FakeBackend>>,
    ids: Arc<BTreeMap<String, RecordId>>,
    notes: BTreeMap<String, Document>,
}

fn system() -> Invocation {
    Invocation {
        site: SiteId::new("perm.test").unwrap(),
        actor: Actor::System {
            component: n("query_permission"),
        },
        command_id: CommandId::new("seed").ok(),
        language: None,
        now: now(),
    }
}

fn parse(id: &str) -> RecordId {
    spot_permissions::ids::parse_record_id(id).unwrap()
}

/// The engine under the permission authorizer, two regions and six notes created as the
/// platform (titles interleave visible and hidden in sort order), and the list
/// adapter over it. The notes are seeded in an order that is not the title order.
async fn rig() -> Rig {
    let ids: BTreeMap<String, RecordId> = CALLERS
        .iter()
        .map(|c| ((*c).to_owned(), RecordId::new_v7()))
        .collect();
    let mut memory = MemoryPrincipals::new();
    for c in CALLERS {
        memory = memory.with(&ids[c], principal(c));
    }
    let meta = Registry::new(definitions()).unwrap().effective();
    let clock = Arc::new(ManualClock::new(now()));
    let engine = Arc::new(
        Engine::new(
            FakeBackend::new(),
            meta.clone(),
            Arc::new(RegistryBuilder::new().build().unwrap()),
            clock.clone(),
            Arc::new(V7Ids),
        )
        .unwrap()
        .with_authorizer(Arc::new(PermissionAuthorizer::new(
            decision_engine(),
            Arc::new(memory),
        ))),
    );
    engine.prepare().await.unwrap();
    let seed = system();
    for region in [ALLOWED_REGION, OTHER_REGION] {
        let mut doc = Document::new(n("region"), parse(region));
        doc.set(n("name"), region);
        engine.create(&seed, doc).await.unwrap();
    }
    let mut notes = BTreeMap::new();
    for (title, region) in [
        (HIDDEN[1], OTHER_REGION),
        (VISIBLE[2], ALLOWED_REGION),
        (HIDDEN[0], OTHER_REGION),
        (VISIBLE[0], ALLOWED_REGION),
        (HIDDEN[2], OTHER_REGION),
        (VISIBLE[1], ALLOWED_REGION),
    ] {
        let mut doc = Document::new(n("note"), RecordId::new_v7());
        doc.set(n("title"), title);
        doc.set(n("region"), Value::Id(parse(region)));
        let created = engine.create(&seed, doc).await.unwrap().value;
        notes.insert(title.to_owned(), created);
    }
    let ids = Arc::new(ids);
    let by_token = ids.clone();
    let resources = EngineResources::new(
        engine.clone(),
        meta,
        clock,
        Arc::new(move |c: &RequestCtx| {
            let token = c
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .ok_or_else(|| ApiError::from_code(Code::AuthRequired))?;
            by_token
                .get(&token)
                .map(|id| Actor::User { id: *id, via: None })
                .ok_or_else(|| ApiError::from_code(Code::AuthRequired))
        }),
    );
    Rig {
        engine,
        resources,
        ids,
        notes,
    }
}

fn ctx(token: &str) -> RequestCtx {
    RequestCtx {
        site: SiteId::new("perm.test").unwrap(),
        credential: Some(Credential::new(token)),
        idempotency_key: None,
        language: None,
        request_id: RequestId::generate(),
        peer: None,
    }
}

fn inv(rig: &Rig, who: &str) -> Invocation {
    Invocation {
        actor: Actor::User {
            id: rig.ids[who],
            via: None,
        },
        ..system()
    }
}

fn spec(limit: u32) -> QuerySpec {
    QuerySpec {
        fields: Vec::new(),
        filter: None,
        order: vec![OrderKey {
            field: n("title"),
            dir: OrderDir::Asc,
            nulls: None,
        }],
        limit,
        cursor: None,
        with_total: true,
    }
}

async fn list(rig: &Rig, who: &str, spec: &QuerySpec) -> Result<Page, ApiError> {
    rig.resources.list(&ctx(who), "note", spec).await
}

/// What the caller received, as text: documents, cursor and total.
fn shown(page: &Page) -> String {
    format!("{page:?}")
}

/// The visible titles found in a page, in page order.
fn titles(page: &Page) -> Vec<&'static str> {
    let text = shown(page);
    let mut found: Vec<(usize, &'static str)> = VISIBLE
        .iter()
        .chain(HIDDEN.iter())
        .filter_map(|t| text.find(t).map(|at| (at, *t)))
        .collect();
    found.sort_by_key(|(at, _)| *at);
    found.into_iter().map(|(_, t)| t).collect()
}

fn assert_no_trace(who: &str, what: &str, text: &str) {
    for marker in HIDDEN {
        assert!(
            !text.contains(marker),
            "{who}: {what} shows `{marker}`: {text}"
        );
    }
    assert!(
        !text.contains(OTHER_REGION),
        "{who}: {what} shows the other region: {text}"
    );
}

#[tokio::test]
async fn a_full_list_has_only_the_rows_the_caller_may_read() {
    let rig = rig().await;

    // Positive control: a caller with full read sees all six, and the total says six.
    let all = list(&rig, "full", &spec(50)).await.unwrap();
    assert_eq!(all.documents.len(), 6);
    assert_eq!(all.total, Some(6));
    assert_eq!(
        titles(&all),
        [
            "a-note-1",
            "b-secret-1",
            "c-note-2",
            "d-secret-2",
            "e-note-3",
            "f-secret-3"
        ]
    );

    // The scoped caller: three rows, total three, no cursor, nothing of the rest.
    let page = list(&rig, "scoped", &spec(50)).await.unwrap();
    assert_eq!(page.documents.len(), 3, "{}", shown(&page));
    assert_eq!(page.total, Some(3));
    assert!(page.next_cursor.is_none());
    assert_eq!(titles(&page), VISIBLE);
    assert_no_trace("scoped", "the list", &shown(&page));

    // A caller with no read right at all: an empty page and a zero total, not an error.
    let none = list(&rig, "nobody", &spec(50)).await.unwrap();
    assert!(none.documents.is_empty());
    assert_eq!(none.total, Some(0));
    assert!(none.next_cursor.is_none());
}

#[tokio::test]
async fn a_filtered_and_a_sorted_list_stay_inside_the_callers_rows() {
    let rig = rig().await;

    // A filter that matches only hidden rows answers as an empty set, with a zero total.
    let mut by_hidden = spec(50);
    by_hidden.filter = Some(FilterAst::Cmp(Comparison {
        field: n("title"),
        op: Op::Contains,
        values: vec![FilterValue::Text("secret".into())],
        case_insensitive: false,
    }));
    let page = list(&rig, "scoped", &by_hidden).await.unwrap();
    assert!(page.documents.is_empty(), "{}", shown(&page));
    assert_eq!(page.total, Some(0));
    assert_no_trace("scoped", "a filter on hidden titles", &shown(&page));

    // The same filter for the caller with full read matches all three.
    let full = list(&rig, "full", &by_hidden).await.unwrap();
    assert_eq!(full.documents.len(), 3);
    assert_eq!(full.total, Some(3));

    // A filter that matches visible and hidden rows keeps the visible ones.
    let mut by_note_or_secret = spec(50);
    by_note_or_secret.filter = Some(FilterAst::Or(vec![
        by_hidden.filter.clone().unwrap(),
        FilterAst::Cmp(Comparison {
            field: n("title"),
            op: Op::Contains,
            values: vec![FilterValue::Text("note".into())],
            case_insensitive: false,
        }),
    ]));
    let page = list(&rig, "scoped", &by_note_or_secret).await.unwrap();
    assert_eq!(titles(&page), VISIBLE);
    assert_eq!(page.total, Some(3));

    // Descending order.
    let mut desc = spec(50);
    desc.order[0].dir = OrderDir::Desc;
    let page = list(&rig, "scoped", &desc).await.unwrap();
    assert_eq!(titles(&page), ["e-note-3", "c-note-2", "a-note-1"]);
    assert_no_trace("scoped", "a descending list", &shown(&page));
}

#[tokio::test]
async fn paging_with_a_page_size_of_one_never_shows_a_hidden_row_or_a_gap() {
    let rig = rig().await;
    let mut request = spec(1);
    let mut seen = Vec::new();
    let mut pages = 0;
    loop {
        let page = list(&rig, "scoped", &request).await.unwrap();
        pages += 1;
        assert!(pages <= 3, "more pages than visible rows");
        // One row per page: the hidden rows between visible ones are not pages of their own,
        // and the total never moves.
        assert_eq!(page.documents.len(), 1, "{}", shown(&page));
        assert_eq!(page.total, Some(3));
        assert_no_trace("scoped", "a page", &shown(&page));
        seen.extend(titles(&page));
        match page.next_cursor {
            Some(cursor) => {
                assert_no_trace("scoped", "the cursor", &cursor);
                request.cursor = Some(cursor);
            }
            None => break,
        }
    }
    assert_eq!(seen, VISIBLE);
    assert_eq!(pages, 3);

    // The caller with full read pages through all six.
    let mut request = spec(1);
    let mut count = 0;
    loop {
        let page = list(&rig, "full", &request).await.unwrap();
        count += page.documents.len();
        match page.next_cursor {
            Some(cursor) => request.cursor = Some(cursor),
            None => break,
        }
    }
    assert_eq!(count, 6);
}

#[tokio::test]
async fn a_row_removed_mid_paging_does_not_fail_the_next_page() {
    let rig = rig().await;
    let first = list(&rig, "scoped", &spec(1)).await.unwrap();
    assert_eq!(titles(&first), ["a-note-1"]);
    let cursor = first.next_cursor.clone().unwrap();

    // The next visible row is deleted between the two requests.
    let doomed = &rig.notes[VISIBLE[1]];
    rig.engine
        .delete(&system(), "note", doomed.id(), doomed.version().unwrap())
        .await
        .unwrap();

    let mut request = spec(1);
    request.cursor = Some(cursor);
    let second = list(&rig, "scoped", &request).await.unwrap();
    assert_no_trace("scoped", "the page after a delete", &shown(&second));
    assert_eq!(second.total, Some(2), "{}", shown(&second));
    assert!(
        !shown(&second).contains(VISIBLE[1]),
        "a deleted row is gone from the page"
    );
}

#[tokio::test]
async fn a_hidden_row_added_later_changes_nothing_the_caller_sees() {
    let rig = rig().await;
    let before = list(&rig, "scoped", &spec(2)).await.unwrap();

    let mut doc = Document::new(n("note"), RecordId::new_v7());
    doc.set(n("title"), "g-secret-4");
    doc.set(n("region"), Value::Id(parse(OTHER_REGION)));
    rig.engine.create(&system(), doc).await.unwrap();

    let after = list(&rig, "scoped", &spec(2)).await.unwrap();
    assert_eq!(after.total, before.total);
    assert_eq!(after.next_cursor.is_some(), before.next_cursor.is_some());
    assert_eq!(titles(&after), titles(&before));
    assert!(!shown(&after).contains("g-secret-4"));
}

#[tokio::test]
async fn the_form_doctype_is_checked_before_a_lookup_lists_for_it() {
    let rig = rig().await;
    require_readable(&rig.engine, &inv(&rig, "scoped"), "note")
        .await
        .unwrap();
    require_readable(&rig.engine, &inv(&rig, "full"), "note")
        .await
        .unwrap();
    let refused = require_readable(&rig.engine, &inv(&rig, "nobody"), "note")
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Registry(Code::PermissionDenied));
}

#[tokio::test]
async fn a_query_on_a_field_the_caller_may_not_use_is_an_unknown_field() {
    // A field the permission snapshot has no facts for is not usable by anyone below the
    // system, and answers like a field that does not exist.
    let rig = rig().await;
    let mut spec = spec(50);
    spec.order = vec![OrderKey {
        field: n("no_such_field"),
        dir: OrderDir::Asc,
        nulls: None,
    }];
    let error = list(&rig, "scoped", &spec).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Registry(Code::DocumentUnknownField));
    assert_no_trace("scoped", "the refusal", &format!("{error:?}"));
}
