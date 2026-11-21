//! The directory's roster (WP-AI-07.T16): `DurableDirectory::enrolled` lists the enrolled nodes with
//! their status from the in-memory snapshot, so the hub's edge desk sees a revocation.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use std::sync::Arc;

use common::{TestClock, identity, node, request, rig};
use spot_sync_enrol::{EnrolmentStore, MemoryEnrolmentStore, NodeStatus};

fn memory() -> Arc<dyn EnrolmentStore> {
    Arc::new(MemoryEnrolmentStore::new())
}

fn enrol(rig: &common::Rig, name: &str) {
    let issued = rig
        .service
        .create_enrolment("admin", &request(name))
        .unwrap();
    rig.service
        .redeem(
            &issued.code,
            &node(name),
            identity(name).certificate_der(),
            "s",
        )
        .unwrap();
}

#[test]
fn an_empty_directory_lists_nobody() {
    let rig = rig(memory(), TestClock::new());
    assert!(rig.directory.enrolled().is_empty());
}

#[test]
fn a_redeemed_node_is_listed_active_and_a_pending_one_is_not() {
    let rig = rig(memory(), TestClock::new());
    enrol(&rig, "edge-a");
    // A code created but not redeemed has no certificate: it is not an enrolled node yet.
    rig.service
        .create_enrolment("admin", &request("edge-b"))
        .unwrap();
    rig.directory.refresh().unwrap();
    assert_eq!(
        rig.directory.enrolled(),
        vec![(node("edge-a"), NodeStatus::Active)]
    );
}

#[test]
fn the_list_is_ordered_by_node_and_has_one_entry_per_node() {
    let rig = rig(memory(), TestClock::new());
    enrol(&rig, "edge-b");
    enrol(&rig, "edge-a");
    rig.directory.refresh().unwrap();
    let listed = rig.directory.enrolled();
    assert_eq!(
        listed,
        vec![
            (node("edge-a"), NodeStatus::Active),
            (node("edge-b"), NodeStatus::Active),
        ]
    );
}

#[test]
fn a_revocation_shows_at_once_and_after_a_refresh() {
    let rig = rig(memory(), TestClock::new());
    enrol(&rig, "edge-a");
    enrol(&rig, "edge-b");
    rig.service.revoke("admin", &node("edge-a")).unwrap();
    // Through this process: no refresh needed.
    assert_eq!(
        rig.directory.enrolled(),
        vec![
            (node("edge-a"), NodeStatus::Revoked),
            (node("edge-b"), NodeStatus::Active),
        ]
    );
    // And it stays so after the snapshot is reloaded from the store.
    rig.directory.refresh().unwrap();
    assert_eq!(
        rig.directory.enrolled()[0],
        (node("edge-a"), NodeStatus::Revoked)
    );
}

#[test]
fn a_suspension_and_a_resume_show() {
    let rig = rig(memory(), TestClock::new());
    enrol(&rig, "edge-a");
    rig.service.suspend("admin", &node("edge-a")).unwrap();
    assert_eq!(
        rig.directory.enrolled(),
        vec![(node("edge-a"), NodeStatus::Suspended)]
    );
    rig.service.resume("admin", &node("edge-a")).unwrap();
    assert_eq!(
        rig.directory.enrolled(),
        vec![(node("edge-a"), NodeStatus::Active)]
    );
}

#[test]
fn a_directory_too_old_to_trust_lists_nobody() {
    let clock = TestClock::new();
    let rig = rig(memory(), Arc::clone(&clock));
    enrol(&rig, "edge-a");
    rig.directory.refresh().unwrap();
    assert_eq!(rig.directory.enrolled().len(), 1);
    // Past `fail_closed_after` (30 s) without a refresh the directory authenticates nobody, and
    // the roster is empty too.
    assert!(rig.directory.is_fresh());
    clock.advance(31);
    assert!(!rig.directory.is_fresh());
    assert!(rig.directory.enrolled().is_empty());
    rig.directory.refresh().unwrap();
    assert!(rig.directory.is_fresh());
    assert_eq!(rig.directory.enrolled().len(), 1);
}
