//! The ladder rules, linking through the tenant control store, and the environment store.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_environments::{
    AppRef, Environment, EnvironmentSet, EnvironmentStore, MemoryEnvironmentStore, Policy, Role,
};
use spot_tenancy::{ControlStore, MemoryStore, Region, TenantState};

#[test]
fn a_default_ladder_refuses_a_skip() {
    let error = EnvironmentSet::new(
        "acme",
        Region::new("eu-west").unwrap(),
        apps(),
        vec![
            Environment {
                tenant: tenant_id(1),
                role: Role::Dev,
                order: 10,
            },
            Environment {
                tenant: tenant_id(3),
                role: Role::Prod,
                order: 20,
            },
        ],
        Policy::default(),
    )
    .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::INVALID_MEMBER);

    assert!(
        EnvironmentSet::new(
            "acme",
            Region::new("eu-west").unwrap(),
            apps(),
            vec![
                Environment {
                    tenant: tenant_id(1),
                    role: Role::Dev,
                    order: 10,
                },
                Environment {
                    tenant: tenant_id(3),
                    role: Role::Prod,
                    order: 20,
                },
            ],
            Policy { allow_skip: true },
        )
        .is_ok()
    );
}

#[test]
fn a_ladder_starts_at_dev_and_uses_a_rising_order() {
    let error = EnvironmentSet::new(
        "acme",
        Region::new("eu-west").unwrap(),
        apps(),
        vec![
            Environment {
                tenant: tenant_id(2),
                role: Role::Test,
                order: 10,
            },
            Environment {
                tenant: tenant_id(3),
                role: Role::Prod,
                order: 20,
            },
        ],
        Policy::default(),
    )
    .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::INVALID_MEMBER);

    let error = EnvironmentSet::new(
        "acme",
        Region::new("eu-west").unwrap(),
        apps(),
        vec![
            Environment {
                tenant: tenant_id(1),
                role: Role::Dev,
                order: 10,
            },
            Environment {
                tenant: tenant_id(2),
                role: Role::Test,
                order: 10,
            },
        ],
        Policy::default(),
    )
    .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::INVALID_MEMBER);
}

#[test]
fn linking_requires_active_tenants_in_the_sets_region() {
    let store = MemoryStore::new();
    for (number, name) in [(1, "acme-dev"), (2, "acme-test"), (3, "acme-prod")] {
        let mut registering = tenant(number, name, "eu-west");
        registering.state = TenantState::Provisioning;
        let stored = store
            .insert_within(registering, 1_760_000_000_000, u32::MAX)
            .unwrap();
        let mut active = stored.clone();
        active.state = TenantState::Active;
        store.update(active, stored.version).unwrap();
    }
    let set = set(Policy::default());
    set.link(&store, &apps()).unwrap();

    let other_apps = vec![AppRef::new("crm", "2.0.0").unwrap()];
    let error = set.link(&store, &other_apps).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::LINK_MISMATCH);

    let setup = tenant(1, "acme-dev", "us-east");
    let error = set.check_link(&setup, &apps()).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::LINK_MISMATCH);

    let mut suspended = tenant(1, "acme-dev", "eu-west");
    suspended.state = TenantState::Suspended;
    let error = set.check_link(&suspended, &apps()).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::TENANT_NOT_ACTIVE);
}

#[test]
fn the_store_keeps_unique_names_and_versions() {
    let store = MemoryEnvironmentStore::new();
    let first = store.create(set(Policy::default())).unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(store.get("acme").unwrap(), first);
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(
        store.create(set(Policy::default())).unwrap_err().code(),
        spot_environments::codes::STORE_NAME_TAKEN
    );

    let replaced = store.replace(set(Policy { allow_skip: true }), 1).unwrap();
    assert_eq!(replaced.version, 2);
    assert!(replaced.set.policy().allow_skip);
    assert_eq!(
        store.replace(set(Policy::default()), 1).unwrap_err().code(),
        spot_environments::codes::STORE_VERSION_CONFLICT
    );
    assert_eq!(
        store.get("missing").unwrap_err().code(),
        spot_environments::codes::STORE_NOT_FOUND
    );
}
