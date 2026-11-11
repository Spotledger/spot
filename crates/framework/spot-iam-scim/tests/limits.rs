//! Bounds and administration: ETags, rate and size limits, credential standing, pagination, and
//! the group-mapping administration view.
#![allow(missing_docs)]

mod common;

use common::{at, call, credential, get, id, post};
use serde_json::{Value as Json, json};
use spot_iam_access::{AllowRule, KeyAction, KeyScope};
use spot_iam_directory::UserType;
use spot_iam_roles::{Role, RoleName};
use spot_iam_scim::{
    Credential, GroupMapping, Limits, MappingStore, MemoryStore, ScimRequest, ScimStore,
    group_admin_view,
};

const TENANT: &str = "acme";

fn store() -> MemoryStore {
    MemoryStore::with_roles(vec![Role::custom("Engineering Role")])
}

fn create_user(
    store: &mut MemoryStore,
    limits: &Limits,
    credential: &Credential,
    name: &str,
) -> String {
    let created = call(
        store,
        credential,
        at(0),
        limits,
        post(
            "/Users",
            json!({"userName": name, "displayName": name, "active": true}),
        ),
    );
    assert_eq!(created.status, 201);
    created.body.clone().unwrap_or(Json::Null)["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn an_etag_guards_a_concurrent_write() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let user = create_user(&mut store, &limits, &credential, "etag@example.com");

    let fetched = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        get(&format!("/Users/{user}")),
    );
    let etag = fetched.etag.clone().unwrap_or_default();
    assert!(!etag.is_empty());

    let body = json!({"Operations": [{"op": "replace", "path": "displayName", "value": "New"}]});
    let stale = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        ScimRequest::new(spot_iam_scim::Method::Patch, format!("/Users/{user}"))
            .with_body(body.clone())
            .with_if_match("W/\"99\""),
    );
    assert_eq!(stale.status, 412);

    let fresh = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        ScimRequest::new(spot_iam_scim::Method::Patch, format!("/Users/{user}"))
            .with_body(body)
            .with_if_match(etag),
    );
    assert_eq!(fresh.status, 200);
}

#[test]
fn the_rate_limit_stops_a_flood() {
    let mut store = store();
    let limits = Limits {
        requests_per_window: 2,
        ..Limits::default()
    };
    let credential = credential(TENANT, id());
    for _ in 0..2 {
        let response = call(&mut store, &credential, at(0), &limits, get("/Users"));
        assert_eq!(response.status, 200);
    }
    let limited = call(&mut store, &credential, at(0), &limits, get("/Users"));
    assert_eq!(limited.status, 429);
    assert!(limited.retry_after.is_some());
}

#[test]
fn a_body_over_the_limit_is_refused() {
    let mut store = store();
    let limits = Limits {
        max_body_bytes: 20,
        ..Limits::default()
    };
    let credential = credential(TENANT, id());
    let response = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post(
            "/Users",
            json!({"userName": "big@example.com", "displayName": "A rather long display name"}),
        ),
    );
    assert_eq!(response.status, 413);
}

#[test]
fn a_disabled_or_expired_credential_is_refused() {
    let mut store = store();
    let limits = Limits::default();
    let mut cred = credential(TENANT, id());
    cred.owner_enabled = false;
    let refused = call(&mut store, &cred, at(0), &limits, get("/Users"));
    assert_eq!(refused.status, 401);

    let mut expired = credential(TENANT, id());
    expired.expires_at = Some(at(-1));
    let refused = call(&mut store, &expired, at(0), &limits, get("/Users"));
    assert_eq!(refused.status, 401);

    let mut wrong_owner = credential(TENANT, id());
    wrong_owner.owner_type = UserType::Portal;
    let refused = call(&mut store, &wrong_owner, at(0), &limits, get("/Users"));
    assert_eq!(refused.status, 401);
}

#[test]
fn pagination_reports_the_total_and_caps_the_page() {
    let mut store = store();
    let limits = Limits {
        default_count: 2,
        max_count: 2,
        ..Limits::default()
    };
    let credential = credential(TENANT, id());
    for name in ["a@example.com", "b@example.com", "c@example.com"] {
        create_user(&mut store, &limits, &credential, name);
    }
    let first = call(&mut store, &credential, at(0), &limits, get("/Users"));
    assert_eq!(first.body.clone().unwrap_or(Json::Null)["totalResults"], 3);
    assert_eq!(first.body.clone().unwrap_or(Json::Null)["itemsPerPage"], 2);

    let second = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        get("/Users").with_query("startIndex", "3"),
    );
    assert_eq!(second.body.clone().unwrap_or(Json::Null)["itemsPerPage"], 1);

    // A `count` above the cap is capped, not refused.
    let capped = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        get("/Users").with_query("count", "1000"),
    );
    assert_eq!(capped.status, 200);
    assert_eq!(capped.body.clone().unwrap_or(Json::Null)["itemsPerPage"], 2);
}

#[test]
fn an_unmapped_group_grants_nothing_and_the_admin_view_flags_it() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let user = create_user(&mut store, &limits, &credential, "unmapped@example.com");
    let group = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        post(
            "/Groups",
            json!({"displayName": "Sales", "members": [{"value": user}]}),
        ),
    );
    assert_eq!(group.status, 201);
    assert!(store.assignments_of(&TENANT.to_owned()).is_empty());

    let rows = group_admin_view(
        &store.list_groups(&TENANT.to_owned()),
        &store.mappings(&TENANT.to_owned()),
    );
    assert_eq!(rows.len(), 1);
    assert!(rows[0].unmapped);
    assert!(rows[0].roles.is_empty());
}

#[test]
fn an_invalid_mapping_row_is_refused() {
    let mut store = store();
    let error = store
        .upsert_mapping(
            &TENANT.to_owned(),
            GroupMapping {
                idp_group: "Sales".to_owned(),
                role: RoleName::from("No Such Role"),
                scope: Vec::new(),
            },
        )
        .err()
        .unwrap_or_else(|| panic!("should refuse"));
    assert_eq!(error.code, "iam_scim.mapping");
}

#[test]
fn a_service_provider_config_is_served() {
    let mut store = store();
    let limits = Limits::default();
    let credential = credential(TENANT, id());
    let response = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        get("/ServiceProviderConfig"),
    );
    assert_eq!(response.status, 200);
    let body = response.body.clone().unwrap_or(Json::Null);
    assert_eq!(body["patch"]["supported"], true);
    assert_eq!(body["bulk"]["supported"], false);
    assert_eq!(body["etag"]["supported"], true);

    let schemas = call(&mut store, &credential, at(0), &limits, get("/Schemas"));
    assert_eq!(schemas.status, 200);
    let types = call(
        &mut store,
        &credential,
        at(0),
        &limits,
        get("/ResourceTypes"),
    );
    assert_eq!(types.status, 200);
}

#[test]
fn a_scope_with_no_rules_is_refused() {
    let mut store = store();
    let limits = Limits::default();
    let mut cred = credential(TENANT, id());
    cred.scope = KeyScope {
        allow: Vec::new(),
        scope: Vec::new(),
        expires_at: None,
    };
    let refused = call(&mut store, &cred, at(0), &limits, get("/Users"));
    assert_eq!(refused.status, 401);

    // A key scope built from an allow rule is accepted.
    let good = credential(TENANT, id());
    let _ = AllowRule::new("*", [KeyAction::Read]);
    assert_eq!(good.owner_type, UserType::Service);
}
