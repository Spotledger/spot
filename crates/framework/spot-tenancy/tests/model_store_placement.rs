//! State machine, naming, placement policy and the control store.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::{Fixture, cluster, request};
use spot_tenancy::{
    ClusterId, ControlStore, DatabaseId, Edition, Placement, PlacementMap, Region, TenantId,
    TenantName, TenantState, codes,
};

const ALL: [TenantState; 6] = [
    TenantState::Provisioning,
    TenantState::Active,
    TenantState::Suspended,
    TenantState::Moving,
    TenantState::Deprovisioning,
    TenantState::Deleted,
];

#[test]
fn the_transition_table_is_exactly_this() {
    use TenantState::*;
    let allowed = [
        (Provisioning, Active),
        (Provisioning, Deleted),
        (Active, Suspended),
        (Active, Moving),
        (Active, Deprovisioning),
        (Suspended, Active),
        (Suspended, Moving),
        (Suspended, Deprovisioning),
        (Moving, Active),
        (Moving, Suspended),
        (Deprovisioning, Suspended),
        (Deprovisioning, Deleted),
    ];
    for from in ALL {
        for to in ALL {
            assert_eq!(
                from.can_become(to),
                allowed.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
    }
    for to in ALL {
        assert!(!Deleted.can_become(to), "Deleted is terminal");
    }
}

#[test]
fn names_follow_the_rules() {
    for ok in ["acme", "acme-corp", "a1b", "x".repeat(63).as_str()] {
        assert!(TenantName::new(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "ab",
        "Acme",
        "1acme",
        "acme-",
        "ac--me",
        "acme corp",
        "acme;drop",
        "../acme",
        "acme_corp",
        "admin",
        "system",
        &"x".repeat(64),
    ] {
        assert_eq!(
            TenantName::new(bad).unwrap_err().code(),
            codes::INVALID_NAME,
            "{bad}"
        );
    }
    assert!(Region::new("eu west").is_err());
    assert!(Edition::new("Enterprise").is_err());
    assert!(ClusterId::new("c1; drop schema").is_err());
}

#[test]
fn ids_are_uuid_v7_and_the_schema_comes_from_the_id() {
    let a = TenantId::new_v7(1_700_000_000_000, [1; 10]);
    let b = TenantId::new_v7(1_700_000_000_000, [2; 10]);
    assert_eq!(a.as_uuid().get_version_num(), 7);
    assert_ne!(a, b);
    assert!(a.schema_name().starts_with("t_"));
    assert!(
        a.schema_name()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    );
    assert!(TenantId::from_uuid(uuid::Uuid::nil()).is_err());
    let p = Placement {
        cluster: ClusterId::new("c1").unwrap(),
        database: DatabaseId::new("db1").unwrap(),
        schema: "public".into(),
    };
    assert_eq!(
        p.validate_for(&a).unwrap_err().code(),
        codes::INVALID_PLACEMENT,
        "a schema may never be chosen freely"
    );
}

#[test]
fn placement_never_leaves_the_region() {
    let id = TenantId::new_v7(1, [0; 10]);
    let clusters = vec![
        cluster("c1", "db1", "us-east", 100),
        cluster("c2", "db1", "eu-west", 1),
    ];
    let mut load = BTreeMap::new();
    let eu = Region::new("eu-west").unwrap();
    let p = PlacementMap::choose(&id, &eu, &clusters, &load).unwrap();
    assert_eq!(p.cluster.as_str(), "c2");
    // The only eu-west database is full: no fallback to the roomy us-east one.
    load.insert(
        (
            ClusterId::new("c2").unwrap(),
            DatabaseId::new("db1").unwrap(),
        ),
        1,
    );
    let err = PlacementMap::choose(&id, &eu, &clusters, &load).unwrap_err();
    assert_eq!(err.code(), codes::NO_PLACEMENT);
    // Draining databases take nobody.
    let mut drained = clusters.clone();
    drained[1].capacity = 5;
    drained[1].accepting = false;
    load.clear();
    assert_eq!(
        PlacementMap::choose(&id, &eu, &drained, &load)
            .unwrap_err()
            .code(),
        codes::NO_PLACEMENT
    );
    // Unknown region.
    assert!(
        PlacementMap::choose(&id, &Region::new("ap-south").unwrap(), &clusters, &load).is_err()
    );
}

#[test]
fn placement_spreads_and_is_deterministic() {
    let id = TenantId::new_v7(1, [0; 10]);
    let r = Region::new("eu-west").unwrap();
    let clusters = vec![
        cluster("b", "db1", "eu-west", 10),
        cluster("a", "db1", "eu-west", 10),
        cluster("c", "db1", "eu-west", 20),
    ];
    let mut load = BTreeMap::new();
    // Equal load: the lowest id wins.
    assert_eq!(
        PlacementMap::choose(&id, &r, &clusters, &load)
            .unwrap()
            .cluster
            .as_str(),
        "a"
    );
    load.insert(
        (
            ClusterId::new("a").unwrap(),
            DatabaseId::new("db1").unwrap(),
        ),
        5,
    );
    load.insert(
        (
            ClusterId::new("b").unwrap(),
            DatabaseId::new("db1").unwrap(),
        ),
        4,
    );
    load.insert(
        (
            ClusterId::new("c").unwrap(),
            DatabaseId::new("db1").unwrap(),
        ),
        9,
    );
    // Ratios 0.5, 0.4, 0.45: b is least loaded.
    let first = PlacementMap::choose(&id, &r, &clusters, &load).unwrap();
    assert_eq!(first.cluster.as_str(), "b");
    let mut shuffled = clusters.clone();
    shuffled.reverse();
    assert_eq!(
        PlacementMap::choose(&id, &r, &shuffled, &load).unwrap(),
        first,
        "input order does not matter"
    );
}

#[test]
fn provisioning_registers_a_unique_name_with_a_derived_schema() {
    let f = Fixture::new();
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    let t = f.store.get(&plan.tenant).unwrap();
    assert_eq!(t.state, TenantState::Provisioning);
    assert_eq!(t.placement.schema, plan.tenant.schema_name());
    assert_eq!(t.version, 1);
    assert_eq!(
        plane
            .provision(&request("acme", "us-east", 2), "ops", 1_001)
            .unwrap_err()
            .code(),
        codes::NAME_TAKEN
    );
    assert_eq!(
        plane
            .provision(&request("Acme!", "eu-west", 3), "ops", 1_001)
            .unwrap_err()
            .code(),
        codes::INVALID_NAME
    );
    assert_eq!(
        plane
            .provision(&request("beta", "mars", 4), "ops", 1_001)
            .unwrap_err()
            .code(),
        codes::NO_PLACEMENT
    );
    assert_eq!(
        plane
            .provision(&request("beta", "eu-west", 4), "  ", 1_001)
            .unwrap_err()
            .code(),
        codes::INVALID_REQUEST,
        "an anonymous actor is refused"
    );
}

#[test]
fn the_store_enforces_versions_immutability_and_unique_placement() {
    let f = Fixture::new();
    let plane = f.plane();
    let a = f
        .store
        .get(
            &plane
                .provision(&request("alpha", "eu-west", 1), "ops", 10)
                .unwrap()
                .tenant,
        )
        .unwrap();
    // Stale version.
    let mut next = a.clone();
    next.state = TenantState::Active;
    assert_eq!(
        f.store.update(next.clone(), 7).unwrap_err().code(),
        codes::VERSION_CONFLICT
    );
    let ok = f.store.update(next.clone(), a.version).unwrap();
    assert_eq!(ok.version, a.version + 1);
    // The same stale write again loses.
    assert_eq!(
        f.store.update(next, a.version).unwrap_err().code(),
        codes::VERSION_CONFLICT
    );
    // Illegal transition.
    let mut bad = ok.clone();
    bad.state = TenantState::Provisioning;
    assert_eq!(
        f.store.update(bad, ok.version).unwrap_err().code(),
        codes::INVALID_TRANSITION
    );
    // Region, name and placement are immutable outside a move.
    let mut moved = ok.clone();
    moved.region = Region::new("us-east").unwrap();
    assert_eq!(
        f.store.update(moved, ok.version).unwrap_err().code(),
        codes::INVALID_REQUEST
    );
    let mut renamed = ok.clone();
    renamed.name = TenantName::new("hijack").unwrap();
    assert!(f.store.update(renamed, ok.version).is_err());
    let mut replaced = ok.clone();
    replaced.placement.cluster = ClusterId::new("c2").unwrap();
    assert_eq!(
        f.store.update(replaced, ok.version).unwrap_err().code(),
        codes::INVALID_PLACEMENT
    );
    // Two live tenants can never share a schema.
    let b = f
        .store
        .get(
            &plane
                .provision(&request("bravo", "eu-west", 2), "ops", 11)
                .unwrap()
                .tenant,
        )
        .unwrap();
    let mut clash = b.clone();
    clash.id = TenantId::new_v7(99, [9; 10]);
    clash.name = TenantName::new("charlie").unwrap();
    clash.placement = a.placement.clone();
    assert!(f.store.insert_within(clash, 12, u32::MAX).is_err());
}
