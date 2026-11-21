//! Barrier 3 of ADR-083 with the precise guard (WP-IAM-11.T17): when the store gives the role
//! registry, a portal user in a group receives a portal role whose duties are all
//! outside-exposed; a desk role and a role with an unexposed duty are still refused; a store
//! that gives role records only keeps the blind, fail-closed behaviour.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{at, call, credential, id, post};
use serde_json::json;
use spot_iam_directory::{User, UserType};
use spot_iam_roles::{Registry, RoleName};
use spot_iam_scim::mapping::{GroupMapping, MappingStore};
use spot_iam_scim::resource::ScimProfile;
use spot_iam_scim::{Limits, MemoryStore, ScimStore, StoredUser};

const TENANT: &str = "acme";

const REGISTRY: &str = r#"
[[duty]]
name = "view_own_invoices"
version = 1
outside_exposed = true
[[duty.grants]]
doctype = "sales_invoice"
role = "view_own_invoices"
rights = ["read"]

[[duty]]
name = "post_invoices"
version = 1
[[duty.grants]]
doctype = "sales_invoice"
role = "post_invoices"
rights = ["read", "submit"]

[[role]]
name = "Desk Role"
origin = { kind = "custom" }
duties = [{ name = "post_invoices" }]

[[role]]
name = "Portal Viewer"
origin = { kind = "custom" }
kind = "portal"
duties = [{ name = "view_own_invoices" }]

[[role]]
name = "Portal Leaky"
origin = { kind = "custom" }
kind = "portal"
duties = [{ name = "post_invoices" }]
"#;

const GROUPS: [(&str, &str); 3] = [
    ("desk", "Desk Role"),
    ("viewer", "Portal Viewer"),
    ("leaky", "Portal Leaky"),
];

fn registry() -> Registry {
    Registry::from_toml(REGISTRY).unwrap_or_else(|e| panic!("{e}"))
}

fn with_mappings(mut store: MemoryStore) -> MemoryStore {
    for (group, role) in GROUPS {
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

/// Puts the portal user into the group `name` and returns the roles they hold afterwards.
fn join(store: &mut MemoryStore, portal: &str, name: &str, tick: i64) -> Vec<String> {
    let limits = Limits::default();
    let cred = credential(TENANT, id());
    let made = call(
        store,
        &cred,
        at(tick),
        &limits,
        post(
            "/Groups",
            json!({"displayName": name, "members": [{"value": portal}]}),
        ),
    );
    assert_eq!(made.status, 201);
    roles_of(store, portal)
}

#[test]
fn a_registry_store_reports_its_registry() {
    assert!(MemoryStore::with_registry(registry()).registry().is_some());
    assert!(MemoryStore::new().registry().is_none());
    assert!(
        MemoryStore::with_roles(registry().roles().cloned().collect())
            .registry()
            .is_none()
    );
    // The role records of a registry store are the registry's roles.
    let store = MemoryStore::with_registry(registry());
    assert_eq!(store.role_records().len(), 3);
}

#[test]
fn a_portal_user_now_receives_a_portal_role_whose_duties_are_all_exposed() {
    let mut store = with_mappings(MemoryStore::with_registry(registry()));
    let portal = portal_user(&mut store);
    assert_eq!(join(&mut store, &portal, "viewer", 1), ["Portal Viewer"]);
}

#[test]
fn a_desk_role_and_an_unexposed_duty_are_still_refused() {
    let mut store = with_mappings(MemoryStore::with_registry(registry()));
    let portal = portal_user(&mut store);
    assert!(join(&mut store, &portal, "desk", 1).is_empty());
    assert!(join(&mut store, &portal, "leaky", 2).is_empty());
}

#[test]
fn a_store_without_a_registry_stays_blind_and_fails_closed() {
    // Same roles, but only as records: the guard cannot see the duties, so even the portal
    // role whose duty is exposed is refused (the behaviour before the registry was exposed).
    let mut store = with_mappings(MemoryStore::with_roles(
        registry().roles().cloned().collect(),
    ));
    assert!(store.registry().is_none());
    let portal = portal_user(&mut store);
    assert!(join(&mut store, &portal, "viewer", 1).is_empty());
    assert!(join(&mut store, &portal, "desk", 2).is_empty());
}
