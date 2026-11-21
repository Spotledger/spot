//! Conformance: the two scripted client transcripts replayed against the core, and the RFC
//! error cases.
#![allow(missing_docs)]

mod common;

use std::collections::BTreeMap;

use common::{at, call, credential, delete, get, id, post, query_of, substitute, substitute_json};
use serde_json::{Value as Json, json};
use spot_iam_roles::{Role, RoleName};
use spot_iam_scim::{
    GroupMapping, Limits, MappingStore, MemoryStore, Method, ScimRequest, ScimStore,
};
use spot_storage_contract::RecordId;

const TENANT: &str = "acme";

fn store() -> MemoryStore {
    let mut store = MemoryStore::with_roles(vec![
        Role::custom("Engineering Role"),
        Role::custom("Platform Role"),
    ]);
    for (group, role) in [
        ("Engineering", "Engineering Role"),
        ("Platform", "Platform Role"),
    ] {
        store
            .upsert_mapping(
                &TENANT.to_owned(),
                GroupMapping {
                    idp_group: group.to_owned(),
                    role: RoleName::from(role),
                    scope: Vec::new(),
                },
            )
            .unwrap_or_else(|e| panic!("mapping: {e}"));
    }
    store
}

/// Replays a transcript file against `store`, checking each step's status and expected fields,
/// and returning the captured ids.
fn replay(file: &str, store: &mut MemoryStore, limits: &Limits) -> BTreeMap<String, String> {
    let owner = id();
    let credential = credential(TENANT, owner);
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{file}: {e}"));
    let doc: Json = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
    let client = doc["client"].as_str().unwrap_or("client");
    let mut captures: BTreeMap<String, String> = BTreeMap::new();
    let steps = doc["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("{client}: steps"));
    for step in steps {
        let name = step["name"].as_str().unwrap_or("step");
        let method = Method::parse(step["method"].as_str().unwrap_or("GET"))
            .unwrap_or_else(|| panic!("{client}/{name}: method"));
        let path = substitute(step["path"].as_str().unwrap_or("/"), &captures);
        let mut request = ScimRequest::new(method, path);
        for (key, value) in query_of(step) {
            request = request.with_query(&key, value);
        }
        if let Some(body) = step.get("body") {
            request = request.with_body(substitute_json(body, &captures));
        }
        let response = call(store, &credential, at(0), limits, request);
        let expect = &step["expect"];
        let status = expect["status"].as_u64().unwrap_or(200) as u16;
        assert_eq!(
            response.status, status,
            "{client}/{name}: unexpected status, body {:?}",
            response.body
        );
        let body = response.body.clone().unwrap_or(Json::Null);
        if let Some(map) = expect.as_object() {
            for (key, wanted) in map {
                if key == "status" {
                    continue;
                }
                assert_eq!(
                    &body[key], wanted,
                    "{client}/{name}: field `{key}` did not match"
                );
            }
        }
        if let Some(capture) = step.get("capture").and_then(Json::as_str) {
            let captured = body["id"]
                .as_str()
                .unwrap_or_else(|| panic!("{client}/{name}: no id to capture"))
                .to_owned();
            captures.insert(capture.to_owned(), captured);
        }
    }
    captures
}

fn transcript(client: &str, name: &str) -> String {
    format!(
        "{}/tests/transcripts/{client}/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn entra_transcript_replays() {
    let mut store = store();
    let limits = Limits::default();
    replay(&transcript("entra", "provisioning"), &mut store, &limits);
    // The transcript ends with the member removed, so the user is still there and enabled.
    assert_eq!(store.list_users(&TENANT.to_owned()).len(), 1);
    assert_eq!(store.list_groups(&TENANT.to_owned()).len(), 1);
}

#[test]
fn okta_transcript_replays() {
    let mut store = store();
    let limits = Limits::default();
    replay(&transcript("okta", "provisioning"), &mut store, &limits);
    assert_eq!(store.list_users(&TENANT.to_owned()).len(), 1);
    assert_eq!(store.list_groups(&TENANT.to_owned()).len(), 1);
}

#[test]
fn group_membership_produces_an_idp_assignment_and_no_role_change() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let roles_before = store.role_records();

    let created = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post(
            "/Users",
            json!({"userName": "carol@example.com", "displayName": "Carol", "active": true}),
        ),
    );
    assert_eq!(created.status, 201);
    let user_id = created.body.clone().unwrap_or(Json::Null)["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let group = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post(
            "/Groups",
            json!({"displayName": "Engineering", "members": [{"value": user_id}]}),
        ),
    );
    assert_eq!(group.status, 201);

    let assignments = store.assignments_of(&TENANT.to_owned());
    assert_eq!(assignments.len(), 1, "one idp assignment");
    assert_eq!(assignments[0].role.as_str(), "Engineering Role");
    assert_eq!(assignments[0].source, spot_iam_roles::Source::Idp);
    assert_eq!(assignments[0].user, user_id);

    // The mapping decided an assignment; it never touched a role record.
    assert_eq!(store.role_records(), roles_before);
}

#[test]
fn idempotent_recreate_returns_409_with_the_existing_id() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let body = json!({"userName": "dave@example.com", "displayName": "Dave", "active": true});
    let first = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post("/Users", body.clone()),
    );
    assert_eq!(first.status, 201);
    let existing = first.body.clone().unwrap_or(Json::Null)["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let second = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post("/Users", body),
    );
    assert_eq!(second.status, 409);
    let error = second.body.clone().unwrap_or(Json::Null);
    assert_eq!(error["scimType"], "uniqueness");
    assert!(
        error["detail"]
            .as_str()
            .unwrap_or_default()
            .contains(&existing),
        "the 409 names the existing id: {error}"
    );
}

#[test]
fn rfc_error_cases_are_refused_cleanly() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let created = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post(
            "/Users",
            json!({"userName": "erin@example.com", "displayName": "Erin", "active": true}),
        ),
    );
    let user_id = created.body.clone().unwrap_or(Json::Null)["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let bad_filter = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        get("/Users").with_query("filter", "bogus eq \"x\""),
    );
    assert_eq!(bad_filter.status, 400);
    assert_eq!(error_type(&bad_filter), "invalidFilter");

    let bad_path = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        ScimRequest::new(Method::Patch, format!("/Users/{user_id}"))
            .with_body(json!({"Operations": [{"op": "replace", "path": "nope", "value": "x"}]})),
    );
    assert_eq!(bad_path.status, 400);
    assert_eq!(error_type(&bad_path), "invalidPath");

    let bad_value = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        ScimRequest::new(Method::Patch, format!("/Users/{user_id}")).with_body(
            json!({"Operations": [{"op": "replace", "path": "active", "value": "maybe"}]}),
        ),
    );
    assert_eq!(bad_value.status, 400);
    assert_eq!(error_type(&bad_value), "invalidValue");

    let missing = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        get(&format!("/Users/{}", id())),
    );
    assert_eq!(missing.status, 404);

    let no_name = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post("/Users", json!({"displayName": "No Name", "active": true})),
    );
    assert_eq!(no_name.status, 400);

    let unknown = call(&mut store, &credential, at(0), &limits, get("/Widgets"));
    assert_eq!(unknown.status, 404);
}

#[test]
fn delete_deactivates_and_never_removes_the_record() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let created = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post(
            "/Users",
            json!({"userName": "frank@example.com", "displayName": "Frank", "active": true}),
        ),
    );
    let user_id = created.body.clone().unwrap_or(Json::Null)["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let deleted = call(
        &mut store,
        &credential,
        at(10),
        &limits,
        delete(&format!("/Users/{user_id}")),
    );
    assert_eq!(deleted.status, 204);

    let fetched = call(
        &mut store,
        &credential,
        at(11),
        &limits,
        get(&format!("/Users/{user_id}")),
    );
    assert_eq!(fetched.status, 200, "the record is still there");
    assert_eq!(fetched.body.clone().unwrap_or(Json::Null)["active"], false);
}

fn error_type(response: &spot_iam_scim::ScimResponse) -> String {
    response.body.clone().unwrap_or(Json::Null)["scimType"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Unused import guard: `RecordId` is part of the public surface the tests exercise.
#[allow(dead_code)]
fn _record_id_type(_: RecordId) {}
