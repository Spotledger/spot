//! The acknowledgement deadline and retry by command id.

use spot_journal_durability::GateConfig;

use super::support::{REMOTE_HALT, request, rig_with};
use crate::{CommitAnswer, CommitSettings};

#[tokio::test]
async fn a_missed_deadline_is_unknown_and_a_retry_never_ships_again() {
    let gate = GateConfig {
        ack_deadline_millis: 500,
        ..GateConfig::default()
    };
    let settings = CommitSettings {
        ack_deadline_millis: 500,
        ..CommitSettings::default()
    };
    let rig = rig_with(REMOTE_HALT, settings, gate);
    rig.relay.state().down = true;
    let ticket = rig.commit(1);
    rig.flush().await;
    assert_eq!(rig.relay.state().appends(), 1);

    rig.clock.0.advance(600);
    rig.driver.step().await;
    assert_eq!(ticket.wait().await, CommitAnswer::OutcomeUnknown { seq: 1 });

    // The retry is answered from the gate and never submitted again.
    let retry = rig.driver.submit(request(1)).unwrap();
    assert_eq!(retry.wait().await, CommitAnswer::OutcomeUnknown { seq: 1 });
    assert_eq!(rig.relay.state().appends(), 1);

    // The store returns, the original chunk ships, and the next retry learns it was committed.
    rig.relay.state().down = false;
    rig.clock.0.advance(1_000);
    rig.driver.step().await;
    let again = rig.driver.submit(request(1)).unwrap();
    assert_eq!(again.wait().await, CommitAnswer::Acked(1));
    assert_eq!(
        rig.relay.state().appends(),
        2,
        "one failed, one durable, no third"
    );
    assert_eq!(rig.relay.state().held.len(), 1);
}
