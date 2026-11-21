//! A failed local commit, the local-commit deadline and the settings validation.

use spot_journal_durability::{AckGate, GateConfig, TenantDurability};

use super::support::{
    FakeRelay, REMOTE_DEGRADE, REMOTE_HALT, VirtualClock, request, rig, rig_with,
};
use crate::{CommitAnswer, CommitDriver, CommitError, CommitRequest, CommitSettings};

const LOCAL_DEADLINE: u64 = 120_000;

#[tokio::test]
async fn a_queued_newest_request_is_dropped_and_its_sequence_number_is_reused() {
    let rig = rig(REMOTE_DEGRADE);
    let ticket = rig.driver.submit(request(1)).unwrap();
    rig.driver.local_commit_failed(1).unwrap();
    assert_eq!(ticket.wait().await, CommitAnswer::Withdrawn(1));
    let status = rig.driver.status();
    assert_eq!(status.queued, 0);
    assert_eq!(status.withdrawn_shipped, 0);
    assert_eq!(status.withdrawn_shipped_lowest_seq, None);
    rig.flush().await;
    assert_eq!(rig.relay.state().appends(), 0, "nothing was shipped");

    // The retry of the same command takes the released sequence number.
    let retry = rig.commit(1);
    rig.flush().await;
    assert_eq!(retry.wait().await, CommitAnswer::Acked(1));
    assert_eq!(rig.relay.state().held.len(), 1);
}

#[tokio::test]
async fn a_queued_request_with_a_later_one_behind_it_stays_so_the_relay_sees_no_gap() {
    let rig = rig(REMOTE_DEGRADE);
    let first = rig.driver.submit(request(1)).unwrap();
    // Local commits are confirmed in sequence order, so 2 is only submitted while 1 is open.
    let second = rig.driver.submit(request(2)).unwrap();
    rig.driver.local_commit_failed(1).unwrap();
    assert_eq!(first.wait().await, CommitAnswer::Withdrawn(1));
    let status = rig.driver.status();
    assert_eq!(status.queued, 2);
    assert_eq!(status.withdrawn_shipped, 1);
    assert_eq!(status.withdrawn_shipped_lowest_seq, Some(1));
    rig.driver.local_commit_durable(2).unwrap();
    rig.flush().await;
    // The fake relay asserts consecutive positions: one batch of 1 and 2.
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 2)]);
    assert_eq!(second.wait().await, CommitAnswer::Acked(2));
    assert_eq!(rig.recorder.acked(), vec![2]);
}

#[tokio::test]
async fn a_shipped_request_is_counted_and_later_sequence_numbers_keep_their_order() {
    let rig = rig(REMOTE_HALT);
    let ticket = rig.driver.submit(request(1)).unwrap();
    rig.flush().await;
    assert_eq!(rig.relay.state().appends(), 1, "the entry shipped");
    rig.driver.local_commit_failed(1).unwrap();
    assert_eq!(ticket.wait().await, CommitAnswer::Withdrawn(1));
    let status = rig.driver.status();
    assert_eq!(status.withdrawn_shipped, 1);
    assert_eq!(status.withdrawn_shipped_lowest_seq, Some(1));
    assert_eq!(status.queued, 0);

    // The driver keeps working: the next request is seq 2, shipped right after seq 1.
    assert!(matches!(
        rig.driver.submit(request(1)),
        Err(CommitError::SeqOutOfOrder {
            expected: 2,
            offered: 1
        })
    ));
    let next = rig.commit(2);
    rig.flush().await;
    assert_eq!(next.wait().await, CommitAnswer::Acked(2));
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 1), (2, 2, 2)]);
}

#[tokio::test]
async fn a_locally_confirmed_waiting_request_is_withdrawn_by_the_gate_and_the_command_can_retry() {
    let rig = rig(REMOTE_HALT);
    let ticket = rig.commit(1);
    rig.driver.local_commit_failed(1).unwrap();
    assert_eq!(ticket.wait().await, CommitAnswer::Withdrawn(1));
    assert_eq!(rig.driver.status().awaiting_ack, 0);
    assert_eq!(rig.driver.status().withdrawn_shipped, 1);

    // The sequence number is consumed: the retry of the same command id gets seq 2.
    let retry = rig
        .driver
        .submit(CommitRequest {
            command_id: "cmd-1".into(),
            seq: 2,
            entry: b"{\"seq\":2}".to_vec(),
        })
        .unwrap();
    assert_eq!(retry.seq(), 2);
    rig.driver.local_commit_durable(2).unwrap();
    rig.flush().await;
    assert_eq!(retry.wait().await, CommitAnswer::Acked(2));
    assert_eq!(
        rig.recorder.acked(),
        vec![2],
        "seq 1 was never acknowledged"
    );
}

#[tokio::test]
async fn an_acknowledged_request_is_never_withdrawn() {
    let rig = rig(REMOTE_DEGRADE);
    let ticket = rig.commit(1);
    rig.flush().await;
    assert_eq!(ticket.wait().await, CommitAnswer::Acked(1));
    assert_eq!(
        rig.driver.local_commit_failed(1),
        Err(CommitError::UnknownSeq { seq: 1 })
    );
    assert_eq!(
        rig.driver.local_commit_failed(9),
        Err(CommitError::UnknownSeq { seq: 9 })
    );
    assert_eq!(rig.driver.status().withdrawn_shipped, 0);
    assert_eq!(rig.recorder.acked(), vec![1]);
}

#[tokio::test]
async fn a_withdrawn_request_cannot_be_withdrawn_twice() {
    let rig = rig(REMOTE_DEGRADE);
    let _ticket = rig.driver.submit(request(1)).unwrap();
    rig.driver.local_commit_failed(1).unwrap();
    assert_eq!(
        rig.driver.local_commit_failed(1),
        Err(CommitError::UnknownSeq { seq: 1 })
    );
}

#[tokio::test]
async fn the_deadline_is_not_reached_nothing_expires() {
    let rig = rig(REMOTE_DEGRADE);
    let mut ticket = rig.driver.submit(request(1)).unwrap();
    rig.flush().await;
    rig.clock.0.advance(LOCAL_DEADLINE - 10);
    rig.driver.step().await;
    assert_eq!(ticket.try_answer(), None);
    assert_eq!(rig.driver.status().expired_local_commits, 0);
}

#[tokio::test]
async fn a_request_whose_local_commit_never_arrives_expires_and_the_driver_wakes_for_it() {
    let rig = rig(REMOTE_DEGRADE);
    let ticket = rig.driver.submit(request(1)).unwrap();
    rig.flush().await; // t = 10: shipped, nothing queued
    assert_eq!(rig.driver.next_wake_millis(), Some(LOCAL_DEADLINE + 1));
    // Exactly at the deadline it has not expired yet.
    rig.clock.0.advance(LOCAL_DEADLINE - 10);
    rig.driver.step().await;
    assert_eq!(rig.driver.status().expired_local_commits, 0);
    // One millisecond later it has.
    rig.clock.0.advance(1);
    rig.driver.step().await;
    assert_eq!(ticket.wait().await, CommitAnswer::Withdrawn(1));
    let status = rig.driver.status();
    assert_eq!(status.expired_local_commits, 1);
    assert_eq!(status.withdrawn_shipped, 1, "it had been shipped");
    assert_eq!(rig.driver.next_wake_millis(), None);
    assert_eq!(
        rig.driver.local_commit_durable(1),
        Err(CommitError::UnknownSeq { seq: 1 })
    );
}

#[tokio::test]
async fn a_local_commit_arriving_exactly_at_the_deadline_is_taken() {
    let rig = rig(REMOTE_DEGRADE);
    let ticket = rig.driver.submit(request(1)).unwrap();
    rig.clock.0.advance(LOCAL_DEADLINE);
    rig.driver.step().await;
    assert_eq!(rig.driver.status().expired_local_commits, 0);
    rig.driver.local_commit_durable(1).unwrap();
    rig.flush().await;
    assert_eq!(ticket.wait().await, CommitAnswer::Acked(1));
    assert_eq!(rig.driver.status().expired_local_commits, 0);
}

#[tokio::test]
async fn an_expired_request_still_in_the_queue_is_dropped_before_it_ships() {
    let settings = CommitSettings {
        group_window_millis: 10_000_000,
        ..CommitSettings::default()
    };
    let rig = rig_with(REMOTE_DEGRADE, settings, GateConfig::default());
    let ticket = rig.driver.submit(request(1)).unwrap();
    rig.clock.0.advance(LOCAL_DEADLINE + 1);
    rig.driver.step().await;
    assert_eq!(ticket.wait().await, CommitAnswer::Withdrawn(1));
    let status = rig.driver.status();
    assert_eq!(status.expired_local_commits, 1);
    assert_eq!(status.queued, 0);
    assert_eq!(status.withdrawn_shipped, 0);
    assert_eq!(rig.relay.state().appends(), 0);
}

#[tokio::test]
async fn a_run_of_expired_newest_requests_is_dropped_and_the_numbers_are_released() {
    let settings = CommitSettings {
        group_window_millis: 10_000_000,
        ..CommitSettings::default()
    };
    let rig = rig_with(REMOTE_DEGRADE, settings, GateConfig::default());
    let tickets: Vec<_> = (1..=3)
        .map(|seq| rig.driver.submit(request(seq)).unwrap())
        .collect();
    rig.clock.0.advance(LOCAL_DEADLINE + 1);
    rig.driver.step().await;
    for (i, ticket) in tickets.into_iter().enumerate() {
        assert_eq!(ticket.wait().await, CommitAnswer::Withdrawn(i as u64 + 1));
    }
    assert_eq!(rig.driver.status().expired_local_commits, 3);
    assert_eq!(rig.driver.status().queued, 0);
    // The released numbers start again at 1.
    assert!(rig.driver.submit(request(1)).is_ok());
}

#[test]
fn a_local_commit_deadline_below_the_ack_deadline_is_refused() {
    let gate = GateConfig {
        ack_deadline_millis: 500,
        ..GateConfig::default()
    };
    let build = |local_commit_deadline_millis: u64| {
        CommitDriver::new(
            AckGate::new(
                TenantDurability {
                    policy: REMOTE_HALT,
                    journal_target_configured: true,
                },
                gate,
            ),
            FakeRelay::new(1),
            VirtualClock::default(),
            CommitSettings {
                local_commit_deadline_millis,
                ..CommitSettings::default()
            },
        )
    };
    assert!(matches!(
        build(499),
        Err(CommitError::InvalidSetting {
            name: "local_commit_deadline_millis"
        })
    ));
    assert!(build(500).is_ok());
}

#[tokio::test]
async fn the_ack_deadline_comes_from_the_gate_configuration_alone() {
    let gate = GateConfig {
        ack_deadline_millis: 500,
        ..GateConfig::default()
    };
    let settings = CommitSettings {
        group_window_millis: 10_000_000,
        ..CommitSettings::default()
    };
    let rig = rig_with(REMOTE_HALT, settings, gate);
    let _ticket = rig.commit(1);
    assert_eq!(rig.driver.next_wake_millis(), Some(500));
}
