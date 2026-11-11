//! The store down: degrade, halt, flapping and an ambiguous failure.

use spot_journal_durability::GateConfig;

use super::support::{REMOTE_DEGRADE, REMOTE_HALT, request, rig, rig_with};
use crate::{CommitAnswer, CommitSettings};

#[tokio::test]
async fn degrade_acks_on_the_local_commit_exposes_the_rpo_and_reships() {
    let gate = GateConfig {
        rpo_grace_millis: 100,
        ..GateConfig::default()
    };
    let rig = rig_with(REMOTE_DEGRADE, CommitSettings::default(), gate);
    rig.relay.state().down = true;
    let ticket = rig.commit(1);
    rig.flush().await;
    assert_eq!(ticket.wait().await, CommitAnswer::Acked(1), "degraded ack");
    let status = rig.driver.status();
    assert!(status.degraded && status.store_down);
    assert_eq!(status.queued, 1, "the backlog stays queued");
    assert!(status.rpo_breach.is_none(), "inside the grace");

    rig.clock.0.advance(150);
    rig.driver.step().await;
    assert_eq!(
        rig.driver.status().rpo_breach.unwrap().unshipped_from_seq,
        1
    );

    rig.relay.state().down = false;
    rig.clock.0.advance(1_000);
    rig.driver.step().await;
    let status = rig.driver.status();
    assert!(!status.degraded && !status.store_down);
    assert_eq!(status.queued, 0);
    assert!(status.rpo_breach.is_none(), "the backlog shipped");
    assert!(rig.relay.state().held.contains(&1));
}

#[tokio::test]
async fn halt_refuses_new_commits_until_the_store_returns() {
    let rig = rig(REMOTE_HALT);
    rig.relay.state().down = true;
    let held = rig.commit(1);
    rig.flush().await;
    assert!(rig.driver.status().halted);

    let refused = rig.driver.submit(request(2)).unwrap();
    assert_eq!(
        refused.wait().await,
        CommitAnswer::Refused(spot_journal_durability::Refusal::Halted)
    );
    // The refusal consumed no sequence number.
    rig.relay.state().down = false;
    rig.clock.0.advance(1_000);
    rig.driver.step().await;
    assert!(!rig.driver.status().halted);
    assert_eq!(held.wait().await, CommitAnswer::Acked(1));
    let next = rig.commit(2);
    rig.flush().await;
    assert_eq!(next.wait().await, CommitAnswer::Acked(2));
    assert_eq!(rig.recorder.acked(), vec![1, 2]);
}

#[tokio::test]
async fn a_flapping_store_never_gaps_or_repeats_an_entry() {
    let rig = rig(REMOTE_HALT);
    let mut tickets = Vec::new();
    let mut seq = 1;
    for round in 0..4 {
        rig.relay.state().down = round % 2 == 0;
        for _ in 0..3 {
            tickets.push(rig.commit(seq));
            seq += 1;
        }
        rig.flush().await;
        rig.relay.state().down = false;
        rig.clock.0.advance(1_000);
        rig.driver.step().await;
    }
    for ticket in tickets {
        assert!(matches!(ticket.wait().await, CommitAnswer::Acked(_)));
    }
    let held: Vec<u64> = rig.relay.state().held.iter().copied().collect();
    assert_eq!(held, (1..seq).collect::<Vec<_>>());
    assert_eq!(rig.recorder.acked(), (1..seq).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_chunk_written_despite_a_reported_failure_is_adopted_not_shipped_twice() {
    let rig = rig(REMOTE_HALT);
    {
        let mut s = rig.relay.state();
        s.down = true;
        s.write_despite_failure = true;
    }
    let ticket = rig.commit(1);
    rig.flush().await;
    rig.relay.state().down = false;
    rig.clock.0.advance(1_000);
    rig.driver.step().await;
    assert_eq!(ticket.wait().await, CommitAnswer::Acked(1));
    assert_eq!(rig.relay.state().appends(), 1, "never shipped again");
}

#[tokio::test]
async fn a_failure_no_retry_cures_stops_new_commits() {
    let rig = rig(REMOTE_DEGRADE);
    rig.relay.state().fatal = true;
    let _ticket = rig.commit(1);
    rig.flush().await;
    assert_eq!(
        rig.driver.status().stopped,
        Some("journal_relay.split_brain")
    );
    let error = rig.driver.submit(request(2)).unwrap_err();
    assert_eq!(error.code(), "journal_commit.relay_stopped");
    rig.clock.0.advance(10_000);
    rig.driver.step().await;
    assert_eq!(rig.relay.state().appends(), 1, "no retry of a fatal error");
}
