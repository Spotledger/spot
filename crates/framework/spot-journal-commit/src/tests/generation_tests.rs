//! A driver on a later generation: `seq` restarts at 1 and its commits are acknowledged, not
//! dropped by a monotone guard (WP-JRN-04.T5).

use spot_journal_durability::{GateConfig, Policy};

use super::support::{REMOTE_DEGRADE, rig_in_generation};
use crate::{CommitAnswer, CommitSettings};

#[tokio::test]
async fn the_first_commits_of_a_later_generation_are_acknowledged_under_every_policy() {
    for policy in [Policy::Local, Policy::ReplicaSync, REMOTE_DEGRADE] {
        let rig = rig_in_generation(policy, CommitSettings::default(), GateConfig::default(), 2);
        assert_eq!(rig.driver.generation(), 2);
        let first = rig.commit(1);
        let second = rig.commit(2);
        rig.flush().await;
        rig.driver.standby_confirmed(2);
        assert_eq!(first.wait().await, CommitAnswer::Acked(1), "{policy:?}");
        assert_eq!(second.wait().await, CommitAnswer::Acked(2), "{policy:?}");
        assert_eq!(rig.driver.status().rejected_commits, 0, "{policy:?}");
    }
}

#[tokio::test]
async fn a_failed_commit_of_a_later_generation_withdraws_only_that_entry() {
    let rig = rig_in_generation(
        REMOTE_DEGRADE,
        CommitSettings::default(),
        GateConfig::default(),
        2,
    );
    let first = rig.commit(1);
    let second = rig.commit(2);
    rig.driver.local_commit_failed(2).unwrap();
    assert_eq!(second.wait().await, CommitAnswer::Withdrawn(2));
    rig.flush().await;
    assert_eq!(first.wait().await, CommitAnswer::Acked(1));
    assert_eq!(rig.driver.status().rejected_commits, 0);
}
