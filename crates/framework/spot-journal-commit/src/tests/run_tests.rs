//! The run loop on the virtual clock: no real sleeps.

use std::time::Duration;

use super::support::{REMOTE_DEGRADE, rig};
use crate::CommitAnswer;

#[tokio::test]
async fn run_ships_when_the_window_closes_and_stops_on_request() {
    let rig = rig(REMOTE_DEGRADE);
    let driver = rig.driver.clone();
    let runner = tokio::spawn(async move { driver.run().await });
    let ticket = rig.commit(1);
    tokio::task::yield_now().await;
    assert_eq!(rig.relay.state().appends(), 0, "the window is still open");
    rig.clock.0.advance(5);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), ticket.wait())
            .await
            .unwrap(),
        CommitAnswer::Acked(1)
    );
    rig.driver.stop();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();
}
