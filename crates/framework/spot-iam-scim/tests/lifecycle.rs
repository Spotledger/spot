//! Deactivation is `active=false`, and it ends sessions, tokens, keys and invitations at once.
#![allow(missing_docs)]

mod common;

use common::{at, call, credential, id, patch, post};
use serde_json::{Value as Json, json};
use spot_iam_roles::Role;
use spot_iam_scim::{Limits, MemoryStore, parse_record_id};

const TENANT: &str = "acme";

fn store() -> MemoryStore {
    MemoryStore::with_roles(vec![Role::custom("Engineering Role")])
}

fn create(
    store: &mut MemoryStore,
    limits: &Limits,
    credential: &spot_iam_scim::Credential,
) -> String {
    let created = call(
        store,
        credential,
        at(0),
        limits,
        post(
            "/Users",
            json!({"userName": "grace@example.com", "displayName": "Grace", "active": true}),
        ),
    );
    assert_eq!(created.status, 201);
    created.body.clone().unwrap_or(Json::Null)["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn deactivation_ends_everything_at_once_and_reports_it() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let user = create(&mut store, &limits, &credential);
    let record = parse_record_id(&user).unwrap_or_else(|| panic!("id"));

    store.seed_sessions(&TENANT.to_owned(), &record, 3);
    store.seed_refresh_tokens(&TENANT.to_owned(), &record, 2);
    store.seed_api_keys(&TENANT.to_owned(), &record, 1);
    store.seed_invitations(&TENANT.to_owned(), &record, 4);
    store.seed_open_tasks(&TENANT.to_owned(), &record, 2);

    let response = call(
        &mut store,
        &credential,
        at(100),
        &limits,
        patch(
            &format!("/Users/{user}"),
            json!({"Operations": [{"op": "replace", "path": "active", "value": false}]}),
        ),
    );
    assert_eq!(response.status, 200);
    let outcome = response.outcome.clone().unwrap_or_default();
    assert_eq!(outcome.sessions_ended, 3);
    assert_eq!(outcome.refresh_tokens_revoked, 2);
    assert_eq!(outcome.api_keys_disabled, 1);
    assert_eq!(outcome.invitations_cancelled, 4);
    assert_eq!(outcome.tasks_reassigned, 2);
    assert!(
        outcome.audit_written,
        "the leaver path writes an audit action"
    );

    // Nothing is left running.
    assert_eq!(store.sessions_of(&TENANT.to_owned(), &record), 0);
    assert_eq!(store.refresh_tokens_of(&TENANT.to_owned(), &record), 0);
    assert_eq!(store.api_keys_of(&TENANT.to_owned(), &record), 0);
    assert_eq!(store.invitations_of(&TENANT.to_owned(), &record), 0);

    // The record is kept, disabled.
    let fetched = call(
        &mut store,
        &credential,
        at(101),
        &limits,
        common::get(&format!("/Users/{user}")),
    );
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.body.clone().unwrap_or(Json::Null)["active"], false);

    // The event names every leaver action and carries no secret.
    let disabled: Vec<&spot_iam_scim::ScimEvent> = store
        .events()
        .iter()
        .filter(|e| e.topic == "identity_user_disabled")
        .collect();
    assert_eq!(disabled.len(), 1);
    let actions = disabled[0].data["actions"]
        .as_array()
        .unwrap_or_else(|| panic!("actions"));
    let names: Vec<&str> = actions.iter().filter_map(Json::as_str).collect();
    assert!(names.contains(&"end_sessions"), "{names:?}");
    assert!(names.contains(&"revoke_refresh_tokens"), "{names:?}");
    assert!(names.contains(&"disable_api_keys"), "{names:?}");
    assert!(names.contains(&"cancel_invitations"), "{names:?}");

    assert!(!store.audit_entries().is_empty());
}

#[test]
fn reactivation_does_not_bring_anything_back() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let user = create(&mut store, &limits, &credential);
    let record = parse_record_id(&user).unwrap_or_else(|| panic!("id"));
    store.seed_sessions(&TENANT.to_owned(), &record, 1);

    let off = call(
        &mut store,
        &credential,
        at(10),
        &limits,
        patch(
            &format!("/Users/{user}"),
            json!({"Operations": [{"op": "replace", "path": "active", "value": false}]}),
        ),
    );
    assert_eq!(off.status, 200);

    let on = call(
        &mut store,
        &credential,
        at(20),
        &limits,
        patch(
            &format!("/Users/{user}"),
            json!({"Operations": [{"op": "replace", "path": "active", "value": true}]}),
        ),
    );
    assert_eq!(on.status, 200);
    assert_eq!(on.body.clone().unwrap_or(Json::Null)["active"], true);
    assert_eq!(
        store.sessions_of(&TENANT.to_owned(), &record),
        0,
        "sessions do not come back"
    );
    assert!(on.outcome.is_none(), "enabling runs no leaver actions");
}

#[test]
fn deactivating_twice_is_safe() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let user = create(&mut store, &limits, &credential);
    let body = json!({"Operations": [{"op": "replace", "path": "active", "value": false}]});
    let first = call(
        &mut store,
        &credential,
        at(1),
        &limits,
        patch(&format!("/Users/{user}"), body.clone()),
    );
    assert_eq!(first.status, 200);
    let second = call(
        &mut store,
        &credential,
        at(2),
        &limits,
        patch(&format!("/Users/{user}"), body),
    );
    assert_eq!(second.status, 200);
    assert!(
        second.outcome.is_none(),
        "the second deactivation is a no-op"
    );
}
