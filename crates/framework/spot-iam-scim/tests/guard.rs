//! The SCIM client as a data channel (WP-SEC-04.T10, `Channel::ApiScim`): hidden attributes are
//! removed, masked ones masked, a filter or sort on an attribute the client may not read is
//! refused like an unknown attribute, a guard error fails closed, groups pass through.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{at, credential, get, id as new_id, patch, post};
use serde_json::{Value as Json, json};
use spot_core::ManualClock;
use spot_field_guard::{
    AuditRecord, BoundedQueue, FieldAccessRecord, FieldGuard, MetaProtection, ProtectionSource,
};
use spot_iam_scim::{
    Limits, MemoryStore, ScimContext, ScimGuard, ScimRequest, ScimResponse, handle, handle_guarded,
};
use spot_meta::{
    BuildOptions, Definition, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec, Registry,
};
use spot_permissions::{ModelTexts, PermissionEngine, Principal, Snapshot, load};

const TENANT: &str = "acme";
const MAIL: &str = "pat.private@example.test";
const NUMBER: &str = "EMP-4815162342";

fn grouped(name: &str, group: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Text);
    f.protection_group = Some(group.to_owned());
    f.masking = Some(MaskSpec::placeholder());
    f
}

fn world() -> FieldGuard {
    let mut user = Definition::new("user");
    user.fields = vec![
        FieldSpec::new("user_name", FieldType::Text),
        FieldSpec::new("full_name", FieldType::Text),
        FieldSpec::new("department", FieldType::Text),
        FieldSpec::new("manager", FieldType::Text),
        FieldSpec::new("external_id", FieldType::Text),
        grouped("email", "contact"),
        grouped("employee_number", "hr"),
    ];
    user.protection_groups = vec![
        ProtectionGroupSpec::new("contact", "Contact"),
        ProtectionGroupSpec::new("hr", "HR"),
    ];
    let meta = Registry::with_options(vec![user], BuildOptions::new())
        .unwrap_or_else(|e| panic!("meta: {e:?}"))
        .effective();
    let source = Arc::new(MetaProtection::new(&meta));
    let mut grants = String::new();
    let mut field_grants = String::new();
    for role in ["Reader", "Masked", "Full"] {
        grants.push_str(&format!(
            "[[entries]]\nrole = \"{role}\"\ndoctype = \"user\"\nrights = [\"read\"]\n"
        ));
    }
    for (role, access) in [("Masked", r#"["read_masked"]"#), ("Full", r#"["read"]"#)] {
        for group in ["contact", "hr"] {
            field_grants.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"user\"\ntarget = {{ group = \"{group}\" }}\naccess = {access}\ndeny = false\n"
            ));
        }
    }
    let model = load(&ModelTexts {
        grants,
        field_grants,
        groups: "[[entries]]\nname = \"contact\"\n[[entries]]\nname = \"hr\"\n".to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("model: {:?}", e.as_slice()));
    let facts = source.protection("user").expect("user facts");
    let snapshot = Snapshot::build(&meta, &model)
        .unwrap_or_else(|e| panic!("{:?}", e.as_slice()))
        .with_field_facts("user", facts.fields.clone())
        .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    FieldGuard::new(
        Arc::new(PermissionEngine::new(Arc::new(snapshot))),
        source,
        Arc::new(BoundedQueue::<FieldAccessRecord>::new(64)),
        Arc::new(BoundedQueue::<AuditRecord>::new(64)),
        Arc::new(ManualClock::new(
            spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
        )),
    )
}

fn guard_for(role: &str) -> ScimGuard {
    ScimGuard::new(world(), Principal::with_roles(role, &[role]))
}

fn user_body() -> Json {
    json!({
        "schemas": [
            "urn:ietf:params:scim:schemas:core:2.0:User",
            "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User"
        ],
        "userName": "pat",
        "displayName": "Pat Example",
        "emails": [{ "value": MAIL, "type": "work", "primary": true }],
        "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User": {
            "employeeNumber": NUMBER,
            "department": "Engineering"
        }
    })
}

/// Creates the user as the system (unguarded) and returns the store and the user's id.
fn seeded() -> (MemoryStore, String) {
    let mut store = MemoryStore::new();
    let cred = credential(TENANT, new_id());
    let limits = Limits::default();
    let mut ctx = ScimContext {
        tenant: TENANT.to_owned(),
        credential: &cred,
        now: at(0),
        limits: &limits,
        store: &mut store,
    };
    let made = handle(post("/scim/v2/Users", user_body()), &mut ctx);
    assert_eq!(made.status, 201, "{:?}", made.body);
    let id = made.body.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    (store, id)
}

fn call(store: &mut MemoryStore, guard: &ScimGuard, request: ScimRequest) -> ScimResponse {
    let cred = credential(TENANT, new_id());
    let limits = Limits::default();
    let mut ctx = ScimContext {
        tenant: TENANT.to_owned(),
        credential: &cred,
        now: at(1),
        limits: &limits,
        store,
    };
    handle_guarded(request, &mut ctx, guard)
}

fn text(response: &ScimResponse) -> String {
    response
        .body
        .as_ref()
        .map(Json::to_string)
        .unwrap_or_default()
}

#[test]
fn the_unguarded_core_shows_everything_so_the_guard_is_what_removes_it() {
    let (mut store, id) = seeded();
    let cred = credential(TENANT, new_id());
    let limits = Limits::default();
    let mut ctx = ScimContext {
        tenant: TENANT.to_owned(),
        credential: &cred,
        now: at(1),
        limits: &limits,
        store: &mut store,
    };
    let plain = handle(get(&format!("/scim/v2/Users/{id}")), &mut ctx);
    assert!(text(&plain).contains(MAIL) && text(&plain).contains(NUMBER));
}

#[test]
fn a_full_reader_sees_every_attribute_and_a_masked_reader_the_masked_form() {
    let (mut store, id) = seeded();
    let path = format!("/scim/v2/Users/{id}");
    let full = call(&mut store, &guard_for("Full"), get(&path));
    assert_eq!(full.status, 200);
    let body = full.body.clone().unwrap();
    assert_eq!(body["emails"][0]["value"], MAIL);
    let enterprise = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";
    assert_eq!(body[enterprise]["employeeNumber"], NUMBER);
    assert_eq!(body["userName"], "pat");

    let masked = call(&mut store, &guard_for("Masked"), get(&path));
    let body = masked.body.clone().unwrap();
    assert_eq!(body["emails"][0]["value"], "***");
    assert_eq!(body["emails"][0]["type"], "work");
    assert_eq!(body[enterprise]["employeeNumber"], "***");
    assert_eq!(body[enterprise]["department"], "Engineering");
    assert!(!text(&masked).contains(MAIL) && !text(&masked).contains(NUMBER));
}

#[test]
fn a_client_with_no_field_grant_loses_the_protected_attributes_and_keeps_the_plain_ones() {
    let (mut store, id) = seeded();
    let reader = guard_for("Reader");
    let one = call(&mut store, &reader, get(&format!("/scim/v2/Users/{id}")));
    let body = one.body.clone().unwrap();
    assert!(body.get("emails").is_none());
    let enterprise = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";
    assert!(body[enterprise].get("employeeNumber").is_none());
    assert_eq!(body[enterprise]["department"], "Engineering");
    assert_eq!(body["userName"], "pat");

    // The list and a PATCH answer follow the same rule.
    let list = call(&mut store, &reader, get("/scim/v2/Users"));
    let renamed = json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
        "Operations": [{ "op": "replace", "path": "displayName", "value": "Pat Renamed" }]
    });
    let patched = call(
        &mut store,
        &reader,
        patch(&format!("/scim/v2/Users/{id}"), renamed),
    );
    assert_eq!(patched.status, 200, "{:?}", patched.body);
    for response in [&list, &patched] {
        assert!(!text(response).contains(MAIL) && !text(response).contains(NUMBER));
    }
    assert!(text(&patched).contains("Pat Renamed"));
}

#[test]
fn a_filter_or_sort_on_an_attribute_the_client_may_not_read_is_refused_like_an_unknown_one() {
    let (mut store, _) = seeded();
    let hidden = r#"emails.value co "private""#;
    let unknown = call(
        &mut store,
        &guard_for("Full"),
        get("/scim/v2/Users").with_query("filter", r#"nosuchattr eq "x""#),
    );
    for role in ["Reader", "Masked"] {
        let refused = call(
            &mut store,
            &guard_for(role),
            get("/scim/v2/Users").with_query("filter", hidden),
        );
        assert_eq!(refused.status, 400, "{role}: {:?}", refused.body);
        assert_eq!(refused.body.as_ref().unwrap()["scimType"], "invalidFilter");
        let by_number = get("/scim/v2/Users").with_query("filter", "employeeNumber pr");
        assert_eq!(call(&mut store, &guard_for(role), by_number).status, 400);
        let sorted = get("/scim/v2/Users").with_query("sortBy", "emails.value");
        assert_eq!(call(&mut store, &guard_for(role), sorted).status, 400);
    }
    assert_eq!(unknown.status, 400);
    // The refusal says no more than an unknown attribute does.
    let refused = call(
        &mut store,
        &guard_for("Reader"),
        get("/scim/v2/Users").with_query("filter", r#"employeeNumber eq "x""#),
    );
    assert!(text(&refused).contains("a user does not have"));
    // A client that may read the attribute unmasked filters on it; a plain attribute is open to all.
    let allowed = call(
        &mut store,
        &guard_for("Full"),
        get("/scim/v2/Users").with_query("filter", hidden),
    );
    assert_eq!(allowed.body.as_ref().unwrap()["totalResults"], 1);
    let plain = get("/scim/v2/Users").with_query("filter", r#"department eq "x""#);
    assert_eq!(call(&mut store, &guard_for("Reader"), plain).status, 200);
}

#[test]
fn a_guard_error_removes_the_attributes_and_a_group_passes_through() {
    let (mut store, id) = seeded();
    // The DocType is unknown to the protection source: every judged attribute is removed.
    let ghost = guard_for("Full").with_doctype("ghost_doctype");
    let body = call(&mut store, &ghost, get(&format!("/scim/v2/Users/{id}")))
        .body
        .unwrap();
    for gone in ["userName", "displayName", "emails"] {
        assert!(body.get(gone).is_none(), "{gone}");
    }
    assert!(body["id"].is_string() && body["schemas"].is_array());

    let group = json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
        "displayName": "Engineering"
    });
    let made = call(
        &mut store,
        &guard_for("Reader"),
        post("/scim/v2/Groups", group),
    );
    assert_eq!(made.status, 201, "{:?}", made.body);
    assert_eq!(made.body.unwrap()["displayName"], "Engineering");
}

#[test]
fn a_bad_credential_learns_nothing_about_attributes() {
    let (mut store, _) = seeded();
    let mut cred = credential(TENANT, new_id());
    cred.owner_enabled = false;
    let limits = Limits::default();
    let mut ctx = ScimContext {
        tenant: TENANT.to_owned(),
        credential: &cred,
        now: at(1),
        limits: &limits,
        store: &mut store,
    };
    let request = get("/scim/v2/Users").with_query("filter", "employeeNumber pr");
    let response = handle_guarded(request, &mut ctx, &guard_for("Reader"));
    assert_eq!(response.status, 401);
}
