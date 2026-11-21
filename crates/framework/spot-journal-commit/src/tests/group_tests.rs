//! Group commit: one round trip per window, tickets released in sequence order.

use spot_journal_durability::GateConfig;

use super::support::{REMOTE_DEGRADE, rig, rig_with};
use crate::{CommitAnswer, CommitSettings};

#[tokio::test]
async fn requests_in_one_window_share_one_round_trip_and_ack_in_order() {
    let rig = rig(REMOTE_DEGRADE);
    let tickets: Vec<_> = (1..=5).map(|seq| rig.commit(seq)).collect();
    rig.flush().await;
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 5)]);
    for (i, ticket) in tickets.into_iter().enumerate() {
        assert_eq!(ticket.wait().await, CommitAnswer::Acked(i as u64 + 1));
    }
    assert_eq!(rig.recorder.acked(), vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn nothing_ships_before_the_window_closes() {
    let rig = rig(REMOTE_DEGRADE);
    let _t = rig.commit(1);
    rig.clock.0.advance(4);
    rig.driver.step().await;
    assert_eq!(rig.relay.state().appends(), 0);
    rig.clock.0.advance(1);
    rig.driver.step().await;
    assert_eq!(rig.relay.state().appends(), 1);
}

#[tokio::test]
async fn a_full_batch_ships_at_once_and_the_rest_waits_for_its_window() {
    let settings = CommitSettings {
        max_batch: 3,
        ..CommitSettings::default()
    };
    let rig = rig_with(REMOTE_DEGRADE, settings, GateConfig::default());
    let tickets: Vec<_> = (1..=7).map(|seq| rig.commit(seq)).collect();
    rig.driver.step().await;
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 3), (2, 4, 6)]);
    rig.flush().await;
    assert_eq!(
        rig.relay.state().attempts,
        vec![(1, 1, 3), (2, 4, 6), (3, 7, 7)]
    );
    for ticket in tickets {
        assert!(matches!(ticket.wait().await, CommitAnswer::Acked(_)));
    }
    assert_eq!(rig.recorder.acked(), (1..=7).collect::<Vec<_>>());
}
