//! Ownership enforcement: the pre-write hook and the replica check over one partition map.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_partition::{
    AssignmentRequest, MemoryPartitionRegistry, OwnerCheck, PartitionError, PartitionRegistry,
    ReplicaView, ScopeKey, WriteGate,
};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

fn scope(key_type: &str, value: &str) -> ScopeKey {
    ScopeKey::new(key_type, value).unwrap()
}

/// A registry where `(warehouse, W7)` belongs to branch 07 from t=1000, open-ended.
fn registry_with_b7() -> MemoryPartitionRegistry {
    let registry = MemoryPartitionRegistry::new();
    registry
        .assign(AssignmentRequest {
            scope: scope("warehouse", "W7"),
            node: node("edge-khi#branch_07"),
            effective_from: at(1_000),
            effective_to: None,
            actor: "admin".to_owned(),
            reason: "branch 7 opens".to_owned(),
            at: at(1_000),
        })
        .unwrap();
    registry
}

#[test]
fn the_owner_may_write_and_another_node_is_refused_with_the_owner_named() {
    let registry = registry_with_b7();
    let check = OwnerCheck::new(Arc::new(registry.map_at(at(2_000))));
    let owner = node("edge-khi#branch_07");
    let warehouse = scope("warehouse", "W7");

    assert!(
        check
            .allow_write(&owner, "stock_move", std::slice::from_ref(&warehouse))
            .is_ok()
    );

    let intruder = node("edge-khi#branch_09");
    let refusal = check
        .allow_write(&intruder, "stock_move", std::slice::from_ref(&warehouse))
        .unwrap_err();
    assert_eq!(refusal.code(), "partition.not_owner");
    assert_eq!(refusal.owner(), Some(&owner));
    assert_eq!(refusal.scope(), Some(&warehouse));
    assert!(
        refusal.to_string().contains("branch_07"),
        "the error names the owner: {refusal}"
    );
    assert_eq!(intruder, *refusal.node());
}

#[test]
fn a_record_with_no_scope_keys_may_be_written_by_any_node() {
    let registry = registry_with_b7();
    let check = OwnerCheck::new(Arc::new(registry.map_at(at(2_000))));
    assert!(
        check
            .allow_write(&node("edge-khi#branch_09"), "customer", &[])
            .is_ok()
    );
}

#[test]
fn an_unassigned_scope_key_is_refused_with_no_owner() {
    let registry = registry_with_b7();
    let check = OwnerCheck::new(Arc::new(registry.map_at(at(2_000))));
    let refusal = check
        .allow_write(
            &node("edge-khi#branch_07"),
            "stock_move",
            &[scope("warehouse", "W9")],
        )
        .unwrap_err();
    assert_eq!(refusal.owner(), None);
    assert!(refusal.to_string().contains("no node owns it"));
}

#[test]
fn the_same_check_refuses_a_replica_from_a_non_owner() {
    let registry = registry_with_b7();
    let check = OwnerCheck::new(Arc::new(registry.map_at(at(2_000))));
    let owner = node("edge-khi#branch_07");
    let record = ReplicaView::new("stock_move", owner.clone(), vec![scope("warehouse", "W7")]);

    assert!(check.allow_replica(&owner, &record).is_ok());

    let intruder = node("edge-khi#branch_09");
    let refusal = check.allow_replica(&intruder, &record).unwrap_err();
    assert_eq!(refusal.owner(), Some(&owner));
    assert!(refusal.to_string().contains("branch_07"));
}

#[test]
fn a_record_with_no_scope_keys_is_owned_by_its_origin() {
    let registry = registry_with_b7();
    let check = OwnerCheck::new(Arc::new(registry.map_at(at(2_000))));
    let origin = node("edge-khi#branch_07");
    let record = ReplicaView::new("customer", origin.clone(), Vec::new());

    assert!(check.allow_replica(&origin, &record).is_ok());
    let refusal = check
        .allow_replica(&node("edge-khi#branch_09"), &record)
        .unwrap_err();
    assert_eq!(refusal.owner(), Some(&origin));
}

#[test]
fn the_pre_write_hook_is_the_same_check() {
    let registry = registry_with_b7();
    let gate: Arc<dyn WriteGate> = Arc::new(OwnerCheck::new(Arc::new(registry.map_at(at(2_000)))));
    let warehouse = scope("warehouse", "W7");
    assert!(
        gate.before_write(
            &node("edge-khi#branch_07"),
            "stock_move",
            std::slice::from_ref(&warehouse)
        )
        .is_ok()
    );
    assert!(
        gate.before_write(&node("edge-khi#branch_09"), "stock_move", &[warehouse])
            .is_err()
    );
}

#[test]
fn the_registry_refuses_two_owners_for_one_key_at_the_same_time() {
    let registry = registry_with_b7();
    let conflict = registry
        .assign(AssignmentRequest {
            scope: scope("warehouse", "W7"),
            node: node("edge-khi#branch_09"),
            effective_from: at(1_500),
            effective_to: None,
            actor: "admin".to_owned(),
            reason: "a second writer".to_owned(),
            at: at(1_500),
        })
        .unwrap_err();
    assert!(matches!(conflict, PartitionError::OwnershipConflict { .. }));
    assert_eq!(conflict.code(), "partition.ownership_conflict");
}

#[test]
fn ownership_is_effective_dated_and_audited() {
    let registry = MemoryPartitionRegistry::new();
    let warehouse = scope("warehouse", "W7");
    registry
        .assign(AssignmentRequest {
            scope: warehouse.clone(),
            node: node("edge-khi#branch_07"),
            effective_from: at(1_000),
            effective_to: Some(at(2_000)),
            actor: "admin".to_owned(),
            reason: "branch 7 owns the warehouse".to_owned(),
            at: at(1_000),
        })
        .unwrap();
    // A different node may take over once the first interval has ended.
    registry
        .assign(AssignmentRequest {
            scope: warehouse.clone(),
            node: node("edge-khi#branch_09"),
            effective_from: at(2_000),
            effective_to: None,
            actor: "admin".to_owned(),
            reason: "ownership migrates to branch 9".to_owned(),
            at: at(2_000),
        })
        .unwrap();

    assert_eq!(
        registry.map_at(at(1_500)).owner(&warehouse),
        Some(&node("edge-khi#branch_07"))
    );
    assert_eq!(
        registry.map_at(at(2_500)).owner(&warehouse),
        Some(&node("edge-khi#branch_09"))
    );
    // Before the first assignment takes effect nothing owns the key.
    assert_eq!(registry.map_at(at(500)).owner(&warehouse), None);

    let history = registry.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].actor, "admin");
    assert_eq!(history[1].version, 2);
}
