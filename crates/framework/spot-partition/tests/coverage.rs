//! The start-up coverage check (WP-EDG-03.T7): every partitioned DocType needs a scope field
//! entry, because the owner check skips a DocType it knows no scope for.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_partition::{MemoryPartitionRegistry, OwnerParticipant, PartitionError, ScopeFields};
use spot_sync_protocol::NodeId;

fn fields() -> ScopeFields {
    ScopeFields::new()
        .with("customer", "warehouse", "name")
        .with("sales_invoice", "warehouse", "number")
}

fn uncovered(error: PartitionError) -> Vec<String> {
    assert_eq!(error.code(), "partition.scope_uncovered");
    match error {
        PartitionError::ScopeUncovered { doctypes } => doctypes,
        other => panic!("not scope_uncovered: {other:?}"),
    }
}

#[test]
fn all_covered_passes() {
    fields()
        .check_covers(&["customer", "sales_invoice"])
        .unwrap();
}

#[test]
fn every_uncovered_doctype_is_named_sorted_and_without_repeats() {
    let error = fields()
        .check_covers(&["zeta", "customer", "alpha", "zeta"])
        .unwrap_err();
    assert!(error.to_string().contains("`alpha`") && error.to_string().contains("`zeta`"));
    assert_eq!(uncovered(error), ["alpha", "zeta"]);
}

#[test]
fn an_empty_list_checks_nothing() {
    ScopeFields::new().check_covers(&[]).unwrap();
}

#[test]
fn a_scope_entry_outside_the_list_is_extra_protection() {
    fields().check_covers(&["customer"]).unwrap();
}

#[test]
fn the_message_names_at_most_twenty_doctypes_and_counts_the_rest() {
    let names: Vec<String> = (0..25).map(|i| format!("dt{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let error = ScopeFields::new().check_covers(&refs).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("`dt19`") && !message.contains("`dt20`"),
        "{message}"
    );
    assert!(message.ends_with("and 5 more"), "{message}");
    assert_eq!(uncovered(error).len(), 25, "the error keeps every name");
}

#[test]
fn the_message_carries_doctype_names_only() {
    let message = ScopeFields::new()
        .check_covers(&["memo"])
        .unwrap_err()
        .to_string();
    assert!(message.contains("`memo`"));
}

#[test]
fn checked_refuses_to_build_with_an_uncovered_doctype_and_names_all_of_them() {
    let node = NodeId::new("edge-khi#branch_07").unwrap();
    let registry = Arc::new(MemoryPartitionRegistry::new());
    let error = OwnerParticipant::checked(node.clone(), registry.clone(), fields(), &["b", "a"])
        .err()
        .unwrap();
    assert_eq!(uncovered(error), ["a", "b"]);
    OwnerParticipant::checked(node, registry, fields(), &["customer"]).unwrap();
}
