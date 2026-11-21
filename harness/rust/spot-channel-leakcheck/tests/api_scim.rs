//! Channel `ApiScim`: `spot-iam-scim` through `handle_guarded`, the public entry point that serves
//! users to a SCIM client (an external system mirroring them out of the platform). The attributes of
//! the planted user hold the corpus, judged as the protected fields of the shared world's DocType
//! (`ScimGuard::with_field`), and everything the client receives (a single resource, a list, the
//! answer to a `PATCH` and to a `POST`, every refusal) goes to the harness.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::{Value as Json, json};
use spot_field_guard::leakcheck::{Corpus, Output, assert_no_leak, find_leaks};
use spot_iam_access::{AllowRule, KeyAction, KeyScope};
use spot_iam_directory::UserType;
use spot_iam_scim::{
    Credential, Limits, MemoryStore, Method, ScimContext, ScimGuard, ScimRequest, ScimResponse,
    handle, handle_guarded,
};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

const TENANT: &str = "acme";
const ENTERPRISE: &str = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";

fn credential() -> Credential {
    Credential {
        key: RecordId::new_v7(),
        owner: RecordId::new_v7(),
        owner_type: UserType::Service,
        owner_enabled: true,
        tenant: TENANT.to_owned(),
        scope: KeyScope {
            allow: vec![AllowRule::new(
                "*",
                [
                    KeyAction::Read,
                    KeyAction::Create,
                    KeyAction::Update,
                    KeyAction::Post,
                    KeyAction::Delete,
                ],
            )],
            scope: Vec::new(),
            expires_at: None,
        },
        expires_at: None,
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_791_417_600).unwrap()
}

/// The user's attributes, judged as the protected fields of the shared world's DocType.
fn scim_guard(core: &Core, who: &str) -> ScimGuard {
    ScimGuard::new(core.guard.clone(), principal(who))
        .with_doctype(DOCTYPE)
        .with_field("username", "number")
        .with_field("displayname", "holder")
        .with_field("name", "profile_json")
        .with_field("emails", "card_number")
        .with_field("employeenumber", "salary")
        .with_field("department", "annual_cost")
        .with_field("externalid", "notes")
}

fn user_body(corpus: &Corpus, user_name: &str) -> Json {
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    json!({
        "schemas": [
            "urn:ietf:params:scim:schemas:core:2.0:User",
            ENTERPRISE
        ],
        "userName": user_name,
        "externalId": PLAIN_NOTE,
        "displayName": value("lookalike"),
        "name": { "familyName": value("in_json") },
        "emails": [{ "value": value("card"), "type": "work", "primary": true }],
        ENTERPRISE: { "employeeNumber": value("salary"), "department": value("annual") }
    })
}

fn request(method: Method, path: &str, body: Option<Json>) -> ScimRequest {
    let request = ScimRequest::new(method, path);
    match body {
        Some(body) => request.with_body(body),
        None => request,
    }
}

fn serve(store: &mut MemoryStore, guard: Option<&ScimGuard>, request: ScimRequest) -> ScimResponse {
    let credential = credential();
    let limits = Limits::default();
    let mut ctx = ScimContext {
        tenant: TENANT.to_owned(),
        credential: &credential,
        now: now(),
        limits: &limits,
        store,
    };
    match guard {
        Some(guard) => handle_guarded(request, &mut ctx, guard),
        None => handle(request, &mut ctx),
    }
}

fn output(response: &ScimResponse) -> Output {
    Output::Json(
        response
            .body
            .as_ref()
            .map(Json::to_string)
            .unwrap_or_default(),
    )
}

/// A store holding the planted user, created as the system (unguarded), and the user's id.
fn seeded(corpus: &Corpus) -> (MemoryStore, String) {
    let mut store = MemoryStore::new();
    let made = serve(
        &mut store,
        None,
        request(
            Method::Post,
            "/scim/v2/Users",
            Some(user_body(corpus, "INV-0001")),
        ),
    );
    assert_eq!(made.status, 201, "{:?}", made.body);
    let id = made.body.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    (store, id)
}

#[test]
fn api_scim_never_leaks() {
    let core = core();
    let corpus = corpus();
    let (mut store, id) = seeded(&corpus);
    let mut cells = 0;
    for name in PRINCIPALS {
        let guard = scim_guard(&core, name);
        let visible = visible_labels(name, false);
        let forbidden = corpus.without(&visible);
        let mut outputs: Vec<Output> = Vec::new();

        let one = serve(
            &mut store,
            Some(&guard),
            request(Method::Get, &format!("/scim/v2/Users/{id}"), None),
        );
        let list = serve(
            &mut store,
            Some(&guard),
            request(Method::Get, "/scim/v2/Users", None),
        );
        let patched = serve(
            &mut store,
            Some(&guard),
            request(
                Method::Patch,
                &format!("/scim/v2/Users/{id}"),
                Some(json!({
                    "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                    "Operations": [{ "op": "replace", "path": "externalId", "value": PLAIN_NOTE }]
                })),
            ),
        );
        let created = serve(
            &mut store,
            Some(&guard),
            request(
                Method::Post,
                "/scim/v2/Users",
                Some(user_body(&corpus, &format!("INV-{name}"))),
            ),
        );
        for response in [&one, &list, &patched, &created] {
            assert!(response.status < 300, "{name}: {:?}", response.body);
            outputs.push(output(response));
        }

        // A filter on a protected attribute is refused unless the client may read it unmasked.
        for (filter, label) in [
            ("employeeNumber pr", "salary"),
            (r#"emails.value sw "4111""#, "card"),
            ("department pr", "annual"),
        ] {
            let response = serve(
                &mut store,
                Some(&guard),
                request(Method::Get, "/scim/v2/Users", None).with_query("filter", filter),
            );
            let allowed = visible.contains(&label);
            assert_eq!(response.status < 300, allowed, "{name}: filter `{filter}`");
            outputs.push(output(&response));
            cells += 1;
        }

        // The positive control: a client that may read an attribute gets the value.
        // (Only the attributes a SCIM user carries: `iban` is a child-row field of another
        // DocType and has no SCIM attribute.)
        for label in visible
            .iter()
            .filter(|l| ["salary", "card", "annual"].contains(l))
        {
            let seen: Vec<String> = find_leaks(&corpus.only(&[label]), &outputs)
                .into_iter()
                .map(|l| l.secret)
                .collect();
            assert!(
                seen.iter().any(|s| s == label),
                "{name}: `{label}` should be served but is not"
            );
        }
        assert_no_leak(&forbidden, &outputs);
    }
    assert!(cells >= PRINCIPALS.len() * 3);
}

#[test]
fn the_unguarded_core_serves_every_value_so_the_harness_is_not_vacuous() {
    let corpus = corpus();
    let (mut store, id) = seeded(&corpus);
    let raw = serve(
        &mut store,
        None,
        request(Method::Get, &format!("/scim/v2/Users/{id}"), None),
    );
    assert!(!find_leaks(&corpus, &[output(&raw)]).is_empty());
}

#[test]
fn the_pending_entry_for_api_scim_is_gone() {
    use spot_channel_leakcheck::{Coverage, coverage_of};
    use spot_field_guard::Channel;
    assert_eq!(
        coverage_of(Channel::ApiScim),
        Some(Coverage::Test {
            test: "api_scim_never_leaks",
            producer: "spot-iam-scim"
        })
    );
}
