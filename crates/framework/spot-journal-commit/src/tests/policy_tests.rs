//! Per-policy happy paths and the local-durability ordering rule.

use spot_journal_durability::Policy;

use super::support::{REMOTE_DEGRADE, rig};
use crate::CommitAnswer;

#[tokio::test]
async fn local_acks_on_the_local_commit_alone() {
    let rig = rig(Policy::Local);
    let mut ticket = rig.commit(1);
    assert_eq!(ticket.try_answer(), Some(CommitAnswer::Acked(1)));
}

#[tokio::test]
async fn replica_sync_acks_only_after_the_standby_confirms() {
    let rig = rig(Policy::ReplicaSync);
    let mut ticket = rig.commit(1);
    rig.flush().await;
    assert_eq!(ticket.try_answer(), None);
    rig.driver.standby_confirmed(1);
    assert_eq!(ticket.wait().await, CommitAnswer::Acked(1));
}

#[tokio::test]
async fn remote_sync_acks_only_after_the_chunk_is_durable() {
    let rig = rig(REMOTE_DEGRADE);
    let mut ticket = rig.commit(1);
    assert_eq!(ticket.try_answer(), None, "no chunk yet");
    rig.flush().await;
    assert_eq!(ticket.wait().await, CommitAnswer::Acked(1));
    assert!(rig.relay.state().held.contains(&1));
}

#[tokio::test]
async fn a_durable_chunk_never_acks_before_the_local_commit() {
    let rig = rig(REMOTE_DEGRADE);
    let mut ticket = rig.driver.submit(super::support::request(1)).unwrap();
    rig.flush().await;
    assert!(rig.relay.state().held.contains(&1), "the chunk is durable");
    assert_eq!(ticket.try_answer(), None, "but the local commit is not");
    rig.driver.local_commit_durable(1).unwrap();
    assert_eq!(ticket.wait().await, CommitAnswer::Acked(1));
}

#[tokio::test]
async fn the_ticket_reports_unknown_when_the_driver_is_dropped() {
    let rig = rig(REMOTE_DEGRADE);
    let ticket = rig.commit(1);
    drop(rig);
    assert_eq!(ticket.wait().await, CommitAnswer::OutcomeUnknown { seq: 1 });
}
