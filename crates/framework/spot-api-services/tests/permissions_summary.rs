//! `GET /api/v1/auth/permissions` (WP-UI-01.T1): the summary equals the decision core's answers
//! for six principals over five DocTypes, names no protection group, and answers `304`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod form_support;

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::Request;
use form_support::{
    ADMIN, CUSTOMER_ACME, CUSTOMER_GLOBEX, Forms, NOBODY, OWNER, READER, SCOPED, USER, meta, now,
    permission_engine, principal_of, principals, resolver_for,
};
use serde_json::Value as Json;
use spot_api_services::EngineResources;
use spot_api_services::permissions::{MAX_DOCTYPES, PermissionResources};
use spot_app_api::meta::DocTypeDef;
use spot_core::{ManualClock, SiteId};
use spot_events_contract::lifecycle::Operation;
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::permissions::PermissionSummary;
use spot_http::api::router::{ApiServices, router};
use spot_http::api::service::{
    ApiError, AuthService, BoxFuture, LoginBody, LookupService, PermissionService, RequestCtx,
    Session, SessionInfo,
};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_permissions::{Action, DocFacts, Principal};
use spot_storage_contract::Name;
use tower::ServiceExt;

const DOCTYPES: [&str; 5] = ["invoice", "invoice_line", "customer", "item", "region"];

fn names() -> Vec<Name> {
    DOCTYPES
        .iter()
        .map(|name| Name::new(name).unwrap())
        .collect()
}

fn access_for(user: &'static str) -> Arc<spot_api_services::access::AccessControl> {
    Arc::new(spot_api_services::access::AccessControl::new(
        meta(),
        permission_engine(),
        Arc::new(principals()),
        7,
        Arc::new(ManualClock::new(now())),
        resolver_for(user),
    ))
}

async fn summary_as(user: &'static str, doctypes: &[Name]) -> PermissionSummary {
    PermissionResources::new(access_for(user))
        .summary(&form_support::ctx(), doctypes)
        .await
        .unwrap()
}

fn rights(entry: &spot_http::api::permissions::DoctypePermissions) -> [bool; 8] {
    let r = &entry.rights;
    [
        r.read, r.create, r.write, r.delete, r.submit, r.cancel, r.correct, r.rename,
    ]
}

fn action_of(right: &str) -> Action {
    form_support::action_of(right)
}

/// The optimistic facts the brief describes, built independently of the service: the principal
/// owns the document, no value restriction can block it, and the state is the one the action
/// needs.
fn expected_facts(def: &DocTypeDef, principal: &Principal, action: Action) -> DocFacts {
    let state = if def.is_posted {
        Some(
            match action {
                Action::Op(Operation::Cancel)
                | Action::Op(Operation::UpdatePosted)
                | Action::Correct => "posted",
                _ => "draft",
            }
            .to_owned(),
        )
    } else {
        None
    };
    DocFacts {
        doctype: def.name.as_str().to_owned(),
        id: None,
        created_by: Some(principal.user.clone()),
        state,
        values: std::collections::BTreeMap::new(),
    }
}

#[tokio::test]
async fn the_summary_equals_the_decision_core_for_every_principal_doctype_and_right() {
    let engine = permission_engine();
    let meta = meta();
    for user in [USER, READER, ADMIN, OWNER, SCOPED, NOBODY] {
        let principal = principal_of(user);
        let summary = summary_as(user, &names()).await;
        assert_eq!(summary.doctypes.len(), DOCTYPES.len(), "{user}");
        for (entry, name) in summary.doctypes.iter().zip(DOCTYPES) {
            assert_eq!(entry.doctype.as_str(), name, "{user}");
            let def = meta.doctype(name).unwrap();
            let got = rights(entry);
            for (index, right) in form_support::ACTION_NAMES.iter().enumerate() {
                let action = action_of(right);
                let mut expected = engine
                    .can(
                        &principal,
                        action,
                        &expected_facts(&def, &principal, action),
                    )
                    .allowed;
                // A DocType without a draft stage has no submit, cancel or correct.
                if !def.is_posted && matches!(right, &"submit" | &"cancel" | &"correct") {
                    expected = false;
                }
                assert_eq!(
                    got[index], expected,
                    "{user} on {name}: {right} differs from the decision core"
                );
            }
        }
    }
}

#[tokio::test]
async fn owner_only_and_scoped_grants_are_summarised_optimistically() {
    let engine = permission_engine();

    // The owner grant counts in the summary although no document exists yet ...
    let owner = principal_of(OWNER);
    let summary = summary_as(OWNER, &names()).await;
    let entry = summary
        .doctypes
        .iter()
        .find(|e| e.doctype.as_str() == "invoice")
        .unwrap();
    assert!(
        entry.rights.write,
        "an if_owner grant is optimistic in the summary"
    );

    // ... while the core refuses a document the user did not create.
    let not_owned = DocFacts {
        doctype: "invoice".to_owned(),
        id: Some(form_support::DOC.to_owned()),
        created_by: Some("someone-else".to_owned()),
        state: None,
        values: Default::default(),
    };
    assert!(
        !engine
            .can(&owner, Action::Op(Operation::Update), &not_owned)
            .allowed
    );

    // The scoped principal's summary says write; a document outside the scope is refused.
    let scoped = principal_of(SCOPED);
    let summary = summary_as(SCOPED, &names()).await;
    assert!(summary.doctypes[0].rights.write);
    let outside = DocFacts {
        doctype: "invoice".to_owned(),
        id: Some(form_support::DOC.to_owned()),
        created_by: None,
        state: None,
        values: [(
            "customer".to_owned(),
            spot_storage_contract::Value::Id(form_support::rid(CUSTOMER_GLOBEX)),
        )]
        .into_iter()
        .collect(),
    };
    assert!(
        !engine.can(&scoped, Action::Read, &outside).allowed,
        "the scope still blocks the real decision"
    );
    let inside = DocFacts {
        values: [(
            "customer".to_owned(),
            spot_storage_contract::Value::Id(form_support::rid(CUSTOMER_ACME)),
        )]
        .into_iter()
        .collect(),
        ..outside
    };
    assert!(engine.can(&scoped, Action::Read, &inside).allowed);
}

#[tokio::test]
async fn an_unrighted_user_gets_an_empty_rights_set_and_no_default_entries() {
    let requested = summary_as(NOBODY, &names()).await;
    for entry in &requested.doctypes {
        assert_eq!(rights(entry), [false; 8], "{}", entry.doctype.as_str());
    }
    let default = summary_as(NOBODY, &[]).await;
    assert!(default.doctypes.is_empty(), "{:?}", default.doctypes);
}

#[tokio::test]
async fn the_default_list_names_every_doctype_the_user_holds_a_right_on() {
    let summary = summary_as(USER, &[]).await;
    let names: Vec<&str> = summary
        .doctypes
        .iter()
        .map(|entry| entry.doctype.as_str())
        .collect();
    assert_eq!(names, ["customer", "invoice", "invoice_line", "item"]);
}

#[tokio::test]
async fn the_field_summary_hides_masks_and_lists_read_only_fields_in_definition_order() {
    let summary = summary_as(USER, &names()).await;
    let invoice = summary
        .doctypes
        .iter()
        .find(|entry| entry.doctype.as_str() == "invoice")
        .unwrap();
    let fields = &invoice.fields;
    assert_eq!(fields.hidden, ["hidden_note"]);
    assert_eq!(fields.masked, ["secret"]);
    assert!(fields.revealable.is_empty(), "the fixture grants no reveal");
    assert_eq!(
        fields.read_only,
        ["customer_name", "subtotal", "grand_total"]
    );
    // A hidden field appears in no other list, and no group name is ever sent.
    for list in [&fields.read_only, &fields.masked, &fields.revealable] {
        assert!(!list.iter().any(|f| f == "hidden_note"));
    }
    let json = summary.to_json().unwrap();
    let text = json.to_string();
    assert!(!text.contains("pii"), "{text}");
    assert!(!text.contains("internal"), "{text}");
    assert!(!text.contains("internal_notes"), "{text}");
}

#[tokio::test]
async fn a_reader_sees_masked_fields_and_is_read_only_everywhere_else() {
    let summary = summary_as(READER, &names()).await;
    let invoice = &summary.doctypes[0];
    assert!(invoice.rights.read && !invoice.rights.write && !invoice.rights.create);
    assert_eq!(invoice.fields.masked, ["secret"]);
    assert!(invoice.fields.read_only.contains(&"number".to_owned()));
}

#[tokio::test]
async fn an_unknown_doctype_and_too_many_doctypes_are_refused() {
    let access = access_for(USER);
    let service = PermissionResources::new(access);
    let error = service
        .summary(&form_support::ctx(), &[Name::new("nope").unwrap()])
        .await
        .unwrap_err();
    assert_eq!(error.code.as_str(), "meta.unknown_doctype");

    let too_many: Vec<Name> = (0..=MAX_DOCTYPES)
        .map(|i| Name::new(&format!("doctype{i}")).unwrap())
        .collect();
    let error = service
        .summary(&form_support::ctx(), &too_many)
        .await
        .unwrap_err();
    assert_eq!(error.code.as_str(), "api.invalid_request");
}

// --------------------------------------------------------------------------------------
// The route

struct Unused;

impl LookupService for Unused {
    fn search<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
        _: &'a LookupRequest,
    ) -> BoxFuture<'a, Result<LookupPage, ApiError>> {
        Box::pin(async { panic!("lookup out of scope") })
    }
}

impl AuthService for Unused {
    fn login<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>> {
        Box::pin(async { panic!("auth out of scope") })
    }
    fn logout<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async { panic!("auth out of scope") })
    }
    fn session<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>> {
        Box::pin(async { panic!("auth out of scope") })
    }
}

async fn app() -> axum::Router {
    let forms = Forms::new().await;
    let clock = forms.clock.clone();
    let meta = meta();
    let resources = Arc::new(EngineResources::new(
        forms.engine.clone(),
        meta.clone(),
        clock.clone(),
        resolver_for(USER),
    ));
    let services = ApiServices::new(
        Arc::new(spot_api_services::MetaResources::new(meta)),
        resources,
        Arc::new(Unused),
        Arc::new(Unused),
        Arc::new(MemoryIdempotencyStore::new(clock)),
    )
    .with_permissions(Arc::new(PermissionResources::new(access_for(USER))));
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new().add("test.example", SiteId::new("test").unwrap()),
    )
    .site_routes(router(services))
    .build()
}

struct Answer {
    status: u16,
    headers: axum::http::HeaderMap,
    body: Json,
}

async fn get(app: &axum::Router, path: &str, headers: &[(&str, &str)]) -> Answer {
    let mut request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "test.example")
        .header("authorization", "Bearer form-user");
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let body = if bytes.is_empty() {
        Json::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    Answer {
        status,
        headers,
        body,
    }
}

#[tokio::test]
async fn the_route_answers_etag_and_304() {
    let app = app().await;
    let answer = get(&app, "/api/v1/auth/permissions", &[]).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.headers["etag"], "\"7\"");
    assert_eq!(answer.body["etag"], "\"7\"");
    assert_eq!(answer.body["data"]["doctypes"].as_array().unwrap().len(), 4);
    assert_eq!(
        answer.headers["cache-control"], "private, no-cache",
        "another user must never receive this summary from a cache"
    );

    let not_modified = get(
        &app,
        "/api/v1/auth/permissions",
        &[("if-none-match", "\"7\"")],
    )
    .await;
    assert_eq!(not_modified.status, 304);
    assert_eq!(not_modified.headers["etag"], "\"7\"");
    assert!(not_modified.body.is_null());

    let stale = get(
        &app,
        "/api/v1/auth/permissions",
        &[("if-none-match", "\"6\"")],
    )
    .await;
    assert_eq!(stale.status, 200);
}

#[tokio::test]
async fn the_route_names_a_requested_doctype_and_refuses_an_unknown_one() {
    let app = app().await;
    let answer = get(&app, "/api/v1/auth/permissions?doctype=region", &[]).await;
    assert_eq!(answer.status, 200);
    let entry = &answer.body["data"]["doctypes"][0];
    assert_eq!(entry["doctype"], "region");
    for right in form_support::ACTION_NAMES {
        assert_eq!(entry["rights"][right], false);
    }

    let unknown = get(&app, "/api/v1/auth/permissions?doctype=nope", &[]).await;
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.body["error"]["code"], "meta.unknown_doctype");
}

#[tokio::test]
async fn the_route_never_names_a_protection_group_or_another_users_rights() {
    let app = app().await;
    let answer = get(&app, "/api/v1/auth/permissions", &[]).await;
    let text = answer.body.to_string();
    for forbidden in ["pii", "internal_notes", "\"group\"", "\"role\""] {
        assert!(!text.contains(forbidden), "{forbidden} leaked: {text}");
    }
}
