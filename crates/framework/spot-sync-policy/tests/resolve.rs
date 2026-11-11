//! Resolving policies over effective metadata: a runtime-created DocType syncs by configuration.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{declared, doctype, master, table, text};
use spot_sync_policy::{Class, Direction, PolicySet, PolicyTable, resolve};

#[test]
fn a_runtime_created_doctype_with_a_policy_syncs_without_code() {
    // No Rust type names this DocType: it is a definition, as a site creates one at runtime.
    let meta = common::build(vec![doctype("runtime_widget", vec![text("title")])]);
    let set = declared("runtime_widget", master());

    let table = resolve(&meta, &set);
    let policy = table
        .get("runtime_widget")
        .expect("the runtime DocType is resolved");
    assert_eq!(policy.class, Class::Master);
    assert_eq!(policy.direction, Direction::Down);
    assert!(policy.is_synced());
}

#[test]
fn an_undeclared_doctype_is_local() {
    let meta = common::build(vec![doctype("plain", vec![text("title")])]);
    let table = resolve(&meta, &PolicySet::new());
    let policy = table
        .get("plain")
        .expect("every metadata DocType is resolved");
    assert_eq!(policy.class, Class::Local);
    assert_eq!(policy.direction, Direction::None);
    assert!(!policy.is_synced());
}

#[test]
fn a_doctype_the_table_does_not_hold_reads_as_local() {
    let meta = common::build(vec![doctype("plain", vec![text("title")])]);
    let table = resolve(&meta, &PolicySet::new());
    assert_eq!(table.policy("not_here").class, Class::Local);
}

#[test]
fn a_child_table_follows_its_parent() {
    let parent = doctype("parent_doc", vec![table("rows", "child_doc")]);
    let child = common::child("child_doc", vec![text("title")]);
    let meta = common::build(vec![parent, child]);

    let table = resolve(&meta, &declared("parent_doc", master()));
    assert_eq!(table.policy("parent_doc").class, Class::Master);
    // The child carries the parent's policy; a document travels with its child rows.
    assert_eq!(table.policy("child_doc").class, Class::Master);
    assert_eq!(table.policy("child_doc").direction, Direction::Down);
}

#[test]
fn a_declaration_for_an_unknown_doctype_is_recorded() {
    let meta = common::build(vec![doctype("plain", vec![text("title")])]);
    let table: PolicyTable = resolve(&meta, &declared("ghost", master()));
    assert_eq!(table.unknown(), &["ghost".to_owned()]);
    assert!(table.get("ghost").is_none());
}
