//! Barrier 3 of ADR-083 on SCIM: a group edit never gives a portal user a role an outside user
//! may not hold, and the mapping view says which rows would be refused.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{at, call, credential, id, post};
use serde_json::json;
use spot_iam_directory::{User, UserType};
use spot_iam_roles::{OutsideGuard, Role, RoleKind, RoleName};
use spot_iam_scim::mapping::{GroupMapping, MappingStore, portal_mapping_refusals};
use spot_iam_scim::resource::ScimProfile;
use spot_iam_scim::{Limits, MemoryStore, ScimStore, StoredUser};

const TENANT: &str = "acme";

fn portal_role(name: &str) -> Role {
    let mut role = Role::custom(name);
    role.kind = RoleKind::Portal;
    role
}

fn store() -> MemoryStore {
    let mut store =
        MemoryStore::with_roles(vec![Role::custom("Desk Role"), portal_role("Portal Plain")]);
    for (group, role) in [("desk", "Desk Role"), ("portal", "Portal Plain")] {
        store
            .upsert_mapping(
                &TENANT.to_owned(),
                GroupMapping {
                    idp_group: group.to_owned(),
                    role: RoleName::from(role),
                    scope: vec![],
                },
            )
            .unwrap();
    }
    store
}

fn portal_user(store: &mut MemoryStore) -> String {
    let record = id();
    store
        .insert_user(StoredUser {
            tenant: TENANT.to_owned(),
            user: User::new(
                record,
                UserType::Portal,
                "outsider@example.com",
                "Outsider",
                at(0),
            ),
            profile: ScimProfile {
                user_name: "outsider@example.com".to_owned(),
                ..ScimProfile::default()
            },
            version: 1,
            updated_at: at(0),
        })
        .unwrap();
    record.to_string()
}

fn staff_user(
    store: &mut MemoryStore,
    limits: &Limits,
    cred: &spot_iam_scim::Credential,
) -> String {
    let created = call(
        store,
        cred,
        at(0),
        limits,
        post(
            "/Users",
            json!({"userName": "staff@example.com", "displayName": "Staff", "active": true}),
        ),
    );
    assert_eq!(created.status, 201);
    created.body.unwrap()["id"].as_str().unwrap().to_owned()
}

fn roles_of(store: &MemoryStore, user: &str) -> Vec<String> {
    let mut roles: Vec<String> = store
        .assignments_of(&TENANT.to_owned())
        .iter()
        .filter(|a| a.user == user)
        .map(|a| a.role.as_str().to_owned())
        .collect();
    roles.sort();
    roles
}

#[test]
fn a_group_never_gives_a_portal_user_a_desk_role() {
    let mut store = store();
    let limits = Limits::default();
    let cred = credential(TENANT, id());
    let portal = portal_user(&mut store);
    let staff = staff_user(&mut store, &limits, &cred);

    // Both are in the group that maps to a desk role.
    let made = call(
        &mut store,
        &cred,
        at(1),
        &limits,
        post(
            "/Groups",
            json!({"displayName": "desk", "members": [{"value": portal}, {"value": staff}]}),
        ),
    );
    assert_eq!(made.status, 201);
    assert_eq!(roles_of(&store, &staff), ["Desk Role"]);
    assert!(roles_of(&store, &portal).is_empty());

    // A portal role with nothing in it is fine for the portal user.
    let made = call(
        &mut store,
        &cred,
        at(2),
        &limits,
        post(
            "/Groups",
            json!({"displayName": "portal", "members": [{"value": portal}]}),
        ),
    );
    assert_eq!(made.status, 201);
    assert_eq!(roles_of(&store, &portal), ["Portal Plain"]);
}

#[test]
fn the_mapping_view_names_the_rows_a_portal_member_would_hit() {
    let mut store = store();
    let limits = Limits::default();
    let cred = credential(TENANT, id());
    let portal = portal_user(&mut store);
    let _ = call(
        &mut store,
        &cred,
        at(1),
        &limits,
        post(
            "/Groups",
            json!({"displayName": "desk", "members": [{"value": portal}]}),
        ),
    );
    let tenant = TENANT.to_owned();
    let guard = OutsideGuard::from_roles(&store.role_records(), [portal.clone()]);
    let refusals = portal_mapping_refusals(
        &store.mappings(&tenant),
        &store.list_groups(&tenant),
        &store.list_users(&tenant),
        &guard,
    );
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].idp_group, "desk");
    assert_eq!(refusals[0].portal_members, 1);
    assert!(refusals[0].message.contains("Desk Role"));
    assert!(!refusals[0].message.contains(&portal));
}
