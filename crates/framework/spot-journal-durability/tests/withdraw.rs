//! A failed local commit withdraws the commit (WP-JRN-04.T3).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_journal_durability::*;

fn cfg() -> GateConfig {
    GateConfig {
        rpo_grace_millis: 60_000,
        ack_deadline_millis: 30_000,
        retain_outcomes: 100,
    }
}

fn gate(policy: Policy) -> AckGate {
    AckGate::new(
        TenantDurability {
            policy,
            journal_target_configured: true,
        },
        cfg(),
    )
}

fn remote() -> AckGate {
    gate(Policy::RemoteSync {
        on_unreachable: OnUnreachable::Halt,
    })
}

fn commit(g: &mut AckGate, now: u64, seq: Seq, id: &str) -> Vec<Output> {
    g.handle(
        now,
        Event::LocalCommitDurable {
            seq,
            command_id: id.into(),
        },
    )
}

fn fail(g: &mut AckGate, now: u64, seq: Seq, id: &str) -> Vec<Output> {
    g.handle(
        now,
        Event::LocalCommitFailed {
            seq,
            command_id: id.into(),
        },
    )
}

fn chunk(g: &mut AckGate, now: u64, up_to: Seq) -> Vec<Output> {
    g.handle(
        now,
        Event::ChunkDurable {
            batch: BatchId(up_to),
            up_to_seq: up_to,
        },
    )
}

#[test]
fn a_waiting_commit_is_withdrawn_and_never_acknowledged() {
    let mut g = remote();
    assert_eq!(commit(&mut g, 0, 1, "a"), vec![Output::Wait { seq: 1 }]);
    assert_eq!(fail(&mut g, 1, 1, "a"), vec![Output::Withdrawn { seq: 1 }]);
    assert_eq!(g.pending_count(), 0);
    assert_eq!(g.outcome("a"), Some(CommandOutcome::Withdrawn { seq: 1 }));
    // The chunk that covers it later acknowledges nothing.
    assert!(chunk(&mut g, 2, 1).is_empty());
    assert_eq!(g.outcome("a"), Some(CommandOutcome::Withdrawn { seq: 1 }));
    assert_eq!(g.rejected_events(), 0);
}

#[test]
fn an_outcome_unknown_commit_is_withdrawn() {
    let mut g = remote();
    commit(&mut g, 0, 1, "a");
    let out = g.handle(40_000, Event::Tick);
    assert_eq!(out, vec![Output::CommitOutcomeUnknown { seq: 1 }]);
    assert_eq!(
        fail(&mut g, 40_001, 1, "a"),
        vec![Output::Withdrawn { seq: 1 }]
    );
    assert!(chunk(&mut g, 40_002, 1).is_empty());
    assert_eq!(g.outcome("a"), Some(CommandOutcome::Withdrawn { seq: 1 }));
}

#[test]
fn an_acknowledged_commit_is_never_taken_back() {
    let mut g = gate(Policy::Local);
    assert_eq!(
        commit(&mut g, 0, 1, "a"),
        vec![Output::Ack {
            seq: 1,
            basis: AckBasis::Local
        }]
    );
    assert!(fail(&mut g, 1, 1, "a").is_empty());
    assert_eq!(g.rejected_events(), 1);
    assert_eq!(g.outcome("a"), Some(CommandOutcome::Committed { seq: 1 }));
    assert_eq!(
        g.admit("a"),
        Admission::Existing(CommandOutcome::Committed { seq: 1 })
    );
}

#[test]
fn a_chunk_acknowledged_commit_is_not_withdrawn_either() {
    let mut g = remote();
    commit(&mut g, 0, 1, "a");
    assert_eq!(chunk(&mut g, 1, 1).len(), 1);
    assert!(fail(&mut g, 2, 1, "a").is_empty());
    assert_eq!(g.rejected_events(), 1);
    assert_eq!(g.outcome("a"), Some(CommandOutcome::Committed { seq: 1 }));
}

#[test]
fn an_unknown_sequence_number_is_ignored_and_counted() {
    let mut g = remote();
    assert!(fail(&mut g, 0, 7, "ghost").is_empty());
    assert_eq!(g.rejected_events(), 1);
    // The gate did not move: the first real commit is still welcome.
    assert_eq!(commit(&mut g, 1, 1, "a"), vec![Output::Wait { seq: 1 }]);
    // A matching sequence number with a different command id is refused the same way.
    assert!(fail(&mut g, 2, 1, "other").is_empty());
    assert_eq!(g.rejected_events(), 2);
    assert_eq!(g.pending_count(), 1);
}

#[test]
fn failing_twice_is_a_counted_no_op() {
    let mut g = remote();
    commit(&mut g, 0, 1, "a");
    assert_eq!(fail(&mut g, 1, 1, "a").len(), 1);
    assert!(fail(&mut g, 2, 1, "a").is_empty());
    assert_eq!(g.rejected_events(), 1);
}

#[test]
fn a_withdrawn_command_may_commit_again_with_a_new_sequence_number() {
    let mut g = remote();
    commit(&mut g, 0, 1, "a");
    fail(&mut g, 1, 1, "a");
    assert_eq!(g.admit("a"), Admission::Proceed);
    // The withdrawn number is consumed: reusing it is ignored.
    assert!(commit(&mut g, 2, 1, "a").is_empty());
    assert_eq!(g.outcome("a"), Some(CommandOutcome::Withdrawn { seq: 1 }));
    // The next number works and answers the retry.
    assert_eq!(commit(&mut g, 3, 2, "a"), vec![Output::Wait { seq: 2 }]);
    assert_eq!(g.outcome("a"), Some(CommandOutcome::Pending { seq: 2 }));
    assert_eq!(
        g.admit("a"),
        Admission::Existing(CommandOutcome::Pending { seq: 2 })
    );
    let out = chunk(&mut g, 4, 2);
    assert_eq!(
        out,
        vec![Output::Ack {
            seq: 2,
            basis: AckBasis::Chunk
        }]
    );
    assert_eq!(g.outcome("a"), Some(CommandOutcome::Committed { seq: 2 }));
}

#[test]
fn a_withdrawn_commit_between_two_others_leaves_them_alone() {
    let mut g = remote();
    commit(&mut g, 0, 1, "a");
    commit(&mut g, 0, 2, "b");
    commit(&mut g, 0, 3, "c");
    fail(&mut g, 1, 2, "b");
    let out = chunk(&mut g, 2, 3);
    assert_eq!(
        out,
        vec![
            Output::Ack {
                seq: 1,
                basis: AckBasis::Chunk
            },
            Output::Ack {
                seq: 3,
                basis: AckBasis::Chunk
            }
        ]
    );
}

#[test]
fn halted_gate_still_refuses_a_withdrawn_command_retry() {
    let mut g = remote();
    commit(&mut g, 0, 1, "a");
    fail(&mut g, 1, 1, "a");
    g.handle(2, Event::StoreReachable(false));
    assert_eq!(g.admit("a"), Admission::Refuse(Refusal::Halted));
}

#[test]
fn pruning_keeps_the_retried_command_mapping() {
    let mut r = AckGate::new(
        TenantDurability {
            policy: Policy::RemoteSync {
                on_unreachable: OnUnreachable::Halt,
            },
            journal_target_configured: true,
        },
        GateConfig {
            retain_outcomes: 1,
            ..cfg()
        },
    );
    commit(&mut r, 0, 1, "a");
    fail(&mut r, 1, 1, "a");
    commit(&mut r, 2, 2, "a");
    chunk(&mut r, 3, 2);
    commit(&mut r, 4, 3, "b");
    chunk(&mut r, 5, 3);
    // The old withdrawn record was pruned; the retry's mapping survived or went with its own seq,
    // but never points at a pruned entry.
    assert!(matches!(
        r.outcome("a"),
        None | Some(CommandOutcome::Committed { seq: 2 })
    ));
    assert_eq!(r.outcome("b"), Some(CommandOutcome::Committed { seq: 3 }));
}

#[test]
fn the_gate_exposes_its_configuration() {
    let g = AckGate::new(
        TenantDurability {
            policy: Policy::Local,
            journal_target_configured: true,
        },
        GateConfig {
            rpo_grace_millis: 1,
            ack_deadline_millis: 2,
            retain_outcomes: 3,
        },
    );
    assert_eq!(
        g.config(),
        GateConfig {
            rpo_grace_millis: 1,
            ack_deadline_millis: 2,
            retain_outcomes: 3,
        }
    );
}
