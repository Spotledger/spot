//! The size of a batch: the chunk limits of the relay cap a group commit, and an entry no chunk
//! can hold is refused before it is queued.

use spot_journal_relay::tail::encoded_len_upper_bound_for;

use super::support::{REMOTE_DEGRADE, REMOTE_HALT, Rig, request, rig_limited};
use crate::{BatchLimits, CommitError, CommitRequest, CommitSettings};

const NO_CHUNK_LIMIT: u64 = u64::MAX;

fn limits(line_budget: u64) -> BatchLimits {
    BatchLimits {
        line_budget,
        max_chunk_bytes: NO_CHUNK_LIMIT,
    }
}

fn sized(seq: u64, len: usize) -> CommitRequest {
    CommitRequest {
        command_id: format!("cmd-{seq}"),
        seq,
        entry: vec![b'x'; len],
    }
}

impl Rig {
    /// Submits `seq` with an entry of `len` bytes and confirms its local commit.
    fn commit_sized(&self, seq: u64, len: usize) -> crate::CommitTicket {
        let ticket = self.driver.submit(sized(seq, len)).unwrap();
        self.driver.local_commit_durable(seq).unwrap();
        ticket
    }
}

#[tokio::test]
async fn a_batch_of_exactly_the_budget_stays_one_chunk() {
    let rig = rig_limited(REMOTE_DEGRADE, CommitSettings::default(), limits(40));
    let _tickets: Vec<_> = (1..=4).map(|seq| rig.commit_sized(seq, 10)).collect();
    rig.flush().await;
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 4)]);
    assert_eq!(rig.recorder.acked(), vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn one_byte_over_the_budget_splits_into_two_chunks_and_acks_after_both() {
    let rig = rig_limited(REMOTE_DEGRADE, CommitSettings::default(), limits(40));
    let mut tickets: Vec<_> = (1..=4).map(|seq| rig.commit_sized(seq, 10)).collect();
    tickets.push(rig.commit_sized(5, 1));
    // The size cut makes the first batch full: it ships without waiting for the window.
    rig.driver.step().await;
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 4)]);
    assert_eq!(rig.recorder.acked(), vec![1, 2, 3, 4]);
    // The rest waits for its window, then ships as the second chunk.
    rig.flush().await;
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 4), (2, 5, 5)]);
    assert_eq!(rig.recorder.acked(), vec![1, 2, 3, 4, 5]);
    // The sequence numbers are consecutive across the split.
    assert_eq!(
        rig.relay.state().held.iter().copied().collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
}

#[tokio::test]
async fn an_entry_over_the_line_budget_but_within_a_chunk_ships_alone() {
    let rig = rig_limited(REMOTE_DEGRADE, CommitSettings::default(), limits(10));
    let _tickets = [
        rig.commit_sized(1, 50),
        rig.commit_sized(2, 5),
        rig.commit_sized(3, 5),
    ];
    rig.flush().await;
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 1), (2, 2, 3)]);
    assert_eq!(rig.recorder.acked(), vec![1, 2, 3]);
}

#[tokio::test]
async fn the_chunk_size_bound_cuts_a_batch_as_well() {
    // Room for exactly two 10-byte entries by the relay's own size bound.
    let max_chunk_bytes = encoded_len_upper_bound_for(2, 20);
    let rig = rig_limited(
        REMOTE_DEGRADE,
        CommitSettings::default(),
        BatchLimits {
            line_budget: NO_CHUNK_LIMIT,
            max_chunk_bytes,
        },
    );
    let _tickets: Vec<_> = (1..=5).map(|seq| rig.commit_sized(seq, 10)).collect();
    rig.flush().await;
    assert_eq!(
        rig.relay.state().attempts,
        vec![(1, 1, 2), (2, 3, 4), (3, 5, 5)]
    );
    assert_eq!(rig.recorder.acked(), vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn an_entry_no_chunk_can_hold_is_refused_before_it_is_queued() {
    let rig = rig_limited(
        REMOTE_DEGRADE,
        CommitSettings::default(),
        BatchLimits {
            line_budget: 100,
            max_chunk_bytes: 1_000,
        },
    );
    // Any line of the chunk limit's size is over it once the framing is counted.
    let error = rig.driver.submit(sized(1, 1_000)).unwrap_err();
    let CommitError::EntryTooLarge { bytes, limit } = error.clone() else {
        panic!("{error:?}");
    };
    assert_eq!(error.code(), "journal_commit.entry_too_large");
    assert_eq!(limit, 1_000);
    assert_eq!(bytes, encoded_len_upper_bound_for(1, 1_000));
    assert!(bytes > limit);
    // Nothing was queued, shipped or counted, and the sequence number is still free.
    let status = rig.driver.status();
    assert_eq!(status.queued, 0);
    assert_eq!(status.awaiting_ack, 0);
    assert_eq!(status.withdrawn_shipped, 0);
    rig.flush().await;
    assert_eq!(rig.relay.state().appends(), 0);
    let ticket = rig.driver.submit(request(1)).unwrap();
    assert_eq!(ticket.seq(), 1);
}

#[tokio::test]
async fn the_order_holds_across_a_ten_thousand_entry_commit() {
    let budget = 2_000;
    let rig = rig_limited(REMOTE_DEGRADE, CommitSettings::default(), limits(budget));
    let tickets: Vec<_> = (1..=10_000).map(|seq| rig.commit_sized(seq, 37)).collect();
    rig.flush().await;
    let state = rig.relay.state();
    let mut next = 1;
    for (index, &(batch_id, first, last)) in state.attempts.iter().enumerate() {
        assert_eq!(batch_id, u64::try_from(index).unwrap() + 1);
        assert_eq!(first, next, "the batches are consecutive runs of the queue");
        assert!(
            (last - first + 1) * 37 <= budget,
            "batch {batch_id} is over the budget"
        );
        next = last + 1;
    }
    assert_eq!(next, 10_001);
    assert_eq!(state.held.len(), 10_000);
    drop(state);
    assert_eq!(rig.recorder.acked(), (1..=10_000).collect::<Vec<_>>());
    drop(tickets);
}

#[tokio::test]
async fn a_part_that_fails_leaves_its_commits_unacknowledged_and_the_retry_does_not_repeat_parts() {
    let rig = rig_limited(REMOTE_HALT, CommitSettings::default(), limits(40));
    let _tickets: Vec<_> = (1..=8).map(|seq| rig.commit_sized(seq, 10)).collect();
    // The second part fails: the store goes down on the second attempt.
    rig.relay.state().down_after_attempts = Some(2);
    rig.clock.0.advance(10);
    rig.driver.step().await;
    assert_eq!(rig.relay.state().attempts, vec![(1, 1, 4), (2, 5, 8)]);
    assert_eq!(rig.recorder.acked(), vec![1, 2, 3, 4]);
    let status = rig.driver.status();
    assert!(status.store_down);
    assert_eq!(status.queued, 4, "the failed part waits in the queue");
    assert_eq!(status.awaiting_ack, 4);
    // The store returns: the probe finds nothing of the failed part, and it ships once.
    {
        let mut state = rig.relay.state();
        state.down = false;
        state.down_after_attempts = None;
    }
    rig.clock.0.advance(1_000);
    rig.driver.step().await;
    assert_eq!(
        rig.relay.state().attempts,
        vec![(1, 1, 4), (2, 5, 8), (3, 5, 8)]
    );
    assert_eq!(rig.recorder.acked(), (1..=8).collect::<Vec<_>>());
    assert_eq!(
        rig.relay.state().held.iter().copied().collect::<Vec<_>>(),
        (1..=8).collect::<Vec<_>>()
    );
}
