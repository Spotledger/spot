//! Sequence rules and retry by command id.

use spot_journal_durability::Policy;

use super::support::{REMOTE_DEGRADE, request, rig};
use crate::{CommitAnswer, CommitError};

#[tokio::test]
async fn a_request_must_carry_the_next_sequence_number() {
    let rig = rig(REMOTE_DEGRADE);
    let error = rig.driver.submit(request(3)).unwrap_err();
    assert_eq!(
        error,
        CommitError::SeqOutOfOrder {
            expected: 1,
            offered: 3
        }
    );
    assert_eq!(error.code(), "journal_commit.seq_out_of_order");
}

#[tokio::test]
async fn local_commits_are_confirmed_in_order_and_once() {
    let rig = rig(REMOTE_DEGRADE);
    let _a = rig.driver.submit(request(1)).unwrap();
    let _b = rig.driver.submit(request(2)).unwrap();
    assert_eq!(
        rig.driver.local_commit_durable(2),
        Err(CommitError::SeqOutOfOrder {
            expected: 1,
            offered: 2
        })
    );
    rig.driver.local_commit_durable(1).unwrap();
    assert_eq!(
        rig.driver.local_commit_durable(1),
        Err(CommitError::UnknownSeq { seq: 1 })
    );
    assert_eq!(
        rig.driver.local_commit_durable(9),
        Err(CommitError::UnknownSeq { seq: 9 })
    );
}

#[tokio::test]
async fn a_retry_of_a_command_in_flight_shares_the_answer_and_ships_once() {
    let rig = rig(REMOTE_DEGRADE);
    let first = rig.commit(1);
    let retry = rig.driver.submit(request(1)).unwrap();
    rig.flush().await;
    assert_eq!(first.wait().await, CommitAnswer::Acked(1));
    assert_eq!(retry.wait().await, CommitAnswer::Acked(1));
    assert_eq!(rig.relay.state().appends(), 1);
}

#[tokio::test]
async fn a_retry_of_an_acknowledged_command_is_answered_without_shipping_again() {
    let rig = rig(Policy::Local);
    let _first = rig.commit(1);
    let mut again = rig.driver.submit(request(1)).unwrap();
    assert_eq!(again.try_answer(), Some(CommitAnswer::Acked(1)));
    rig.flush().await;
    assert_eq!(rig.relay.state().appends(), 1, "shipped once, never twice");
}
