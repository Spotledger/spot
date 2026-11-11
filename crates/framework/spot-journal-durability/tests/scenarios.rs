//! Scenario and fault-injection tests for the acknowledgement gate (WP-JRN-04.T1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_journal_durability::*;

fn cfg() -> GateConfig {
    GateConfig {
        rpo_grace_millis: 60_000,
        ack_deadline_millis: 30_000,
        retain_outcomes: 100,
    }
}

fn tenant(policy: Policy, target: bool) -> TenantDurability {
    TenantDurability {
        policy,
        journal_target_configured: target,
    }
}

fn remote(on: OnUnreachable) -> AckGate {
    AckGate::new(
        tenant(Policy::RemoteSync { on_unreachable: on }, true),
        cfg(),
    )
}

fn commit(g: &mut AckGate, now: u64, seq: Seq) -> Vec<Output> {
    g.handle(
        now,
        Event::LocalCommitDurable {
            seq,
            command_id: format!("c{seq}"),
        },
    )
}

fn chunk(g: &mut AckGate, now: u64, batch: u64, up_to: Seq) -> Vec<Output> {
    g.handle(
        now,
        Event::ChunkDurable {
            batch: BatchId(batch),
            up_to_seq: up_to,
        },
    )
}

fn acks(out: &[Output]) -> Vec<Seq> {
    out.iter()
        .filter_map(|o| match o {
            Output::Ack { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect()
}

#[test]
fn local_acks_on_the_local_commit_and_never_waits_for_the_store() {
    let mut g = AckGate::new(tenant(Policy::Local, true), cfg());
    let out = commit(&mut g, 0, 1);
    assert_eq!(
        out,
        vec![Output::Ack {
            seq: 1,
            basis: AckBasis::Local
        }]
    );
    // Store trouble changes nothing.
    assert!(g.handle(5, Event::StoreReachable(false)).is_empty());
    let out = commit(&mut g, 6, 2);
    assert_eq!(acks(&out), vec![2]);
    assert!(!g.is_halted() && !g.is_degraded());
    assert_eq!(g.admit("new"), Admission::Proceed);
}

#[test]
fn local_without_a_target_shows_the_standing_warning() {
    let g = AckGate::new(tenant(Policy::Local, false), cfg());
    assert_eq!(
        g.standing_warning(),
        Some(StandingWarning::NoOffsiteHistory)
    );
    let g = AckGate::new(tenant(Policy::Local, true), cfg());
    assert_eq!(g.standing_warning(), None);
    let g = AckGate::new(tenant(Policy::ReplicaSync, false), cfg());
    assert_eq!(g.standing_warning(), None);
}

#[test]
fn replica_sync_waits_for_the_standby_not_the_store() {
    let mut g = AckGate::new(tenant(Policy::ReplicaSync, false), cfg());
    assert_eq!(commit(&mut g, 0, 1), vec![Output::Wait { seq: 1 }]);
    assert_eq!(commit(&mut g, 1, 2), vec![Output::Wait { seq: 2 }]);
    // A chunk or store event does not release a replica-sync commit.
    assert!(chunk(&mut g, 2, 1, 2).is_empty());
    let out = g.handle(3, Event::StandbyConfirmed { seq: 2 });
    assert_eq!(
        out,
        vec![
            Output::Ack {
                seq: 1,
                basis: AckBasis::Standby
            },
            Output::Ack {
                seq: 2,
                basis: AckBasis::Standby
            },
        ]
    );
    // A confirmation that arrives before the commit event acks at once.
    g.handle(4, Event::StandbyConfirmed { seq: 5 });
    assert_eq!(acks(&commit(&mut g, 5, 3)), vec![3]);
}

#[test]
fn replica_sync_standby_silence_reports_outcome_unknown() {
    let mut g = AckGate::new(tenant(Policy::ReplicaSync, true), cfg());
    commit(&mut g, 0, 1);
    let out = g.handle(30_000, Event::Tick);
    assert_eq!(out, vec![Output::CommitOutcomeUnknown { seq: 1 }]);
    assert_eq!(g.outcome("c1"), Some(CommandOutcome::Unknown { seq: 1 }));
}

#[test]
fn remote_sync_group_commit_one_round_trip_releases_the_batch_in_order() {
    let mut g = remote(OnUnreachable::Halt);
    for seq in 1..=5 {
        assert_eq!(commit(&mut g, seq, seq), vec![Output::Wait { seq }]);
    }
    assert_eq!(g.pending_count(), 5);
    // One ChunkDurable (one object-store round trip) covers the whole range.
    let out = chunk(&mut g, 10, 1, 5);
    assert_eq!(acks(&out), vec![1, 2, 3, 4, 5]);
    assert!(out.iter().all(|o| matches!(
        o,
        Output::Ack {
            basis: AckBasis::Chunk,
            ..
        }
    )));
    assert_eq!(g.pending_count(), 0);
}

#[test]
fn remote_sync_never_acks_early_and_a_partial_chunk_releases_only_its_range() {
    let mut g = remote(OnUnreachable::Degrade);
    for seq in 1..=4 {
        commit(&mut g, 0, seq);
    }
    assert_eq!(acks(&chunk(&mut g, 1, 1, 2)), vec![1, 2]);
    assert_eq!(g.pending_count(), 2);
    assert_eq!(acks(&chunk(&mut g, 2, 2, 4)), vec![3, 4]);
    // A repeated chunk acknowledges nothing twice.
    assert!(chunk(&mut g, 3, 2, 4).is_empty());
    // A commit already covered by an earlier chunk would ack at once; a later one waits.
    assert!(acks(&commit(&mut g, 4, 3_000)).is_empty());
}

#[test]
fn halt_refuses_while_the_store_is_down_and_resumes_by_itself() {
    let mut g = remote(OnUnreachable::Halt);
    assert_eq!(g.admit("a"), Admission::Proceed);
    assert_eq!(
        g.handle(1, Event::StoreReachable(false)),
        vec![Output::Refuse(Refusal::Halted)]
    );
    assert!(g.is_halted());
    assert_eq!(g.admit("a"), Admission::Refuse(Refusal::Halted));
    assert_eq!(
        g.handle(2, Event::StoreReachable(true)),
        vec![Output::StoreRecovered]
    );
    assert!(!g.is_halted());
    assert_eq!(g.admit("a"), Admission::Proceed);
}

#[test]
fn halt_store_down_mid_batch_keeps_commits_waiting_then_acks_when_it_returns() {
    let mut g = remote(OnUnreachable::Halt);
    commit(&mut g, 0, 1);
    commit(&mut g, 1, 2);
    g.handle(
        2,
        Event::ChunkFailed {
            batch: BatchId(1),
            reason: FailureReason::Unreachable,
        },
    );
    assert!(g.is_halted());
    assert_eq!(g.pending_count(), 2);
    // The store returns within the deadline and the retried chunk lands.
    g.handle(10_000, Event::StoreReachable(true));
    let out = chunk(&mut g, 10_001, 2, 2);
    assert_eq!(acks(&out), vec![1, 2]);
    assert!(!g.is_halted());
}

#[test]
fn degrade_store_down_at_commit_acks_locally_and_raises_the_breach_after_the_grace() {
    let mut g = remote(OnUnreachable::Degrade);
    let out = g.handle(1_000, Event::StoreReachable(false));
    assert_eq!(out, vec![Output::Degraded { since: 1_000 }]);
    let out = commit(&mut g, 2_000, 1);
    assert_eq!(
        out,
        vec![Output::Ack {
            seq: 1,
            basis: AckBasis::Degraded
        }]
    );
    assert_eq!(g.admit("other"), Admission::Proceed);
    assert!(g.rpo_breach().is_none());
    // Just before the grace: nothing. At the grace: raised once.
    assert!(g.handle(61_999, Event::Tick).is_empty());
    let out = g.handle(62_000, Event::Tick);
    let breach = RpoBreach {
        since: 2_000,
        unshipped_from_seq: 1,
    };
    assert_eq!(out, vec![Output::RpoBreachRaised(breach)]);
    assert!(g.handle(70_000, Event::Tick).is_empty());
    assert_eq!(g.rpo_breach(), Some(breach));
}

#[test]
fn degrade_releases_commits_waiting_when_the_store_goes_down() {
    let mut g = remote(OnUnreachable::Degrade);
    commit(&mut g, 0, 1);
    commit(&mut g, 0, 2);
    let out = g.handle(5, Event::StoreReachable(false));
    assert_eq!(
        out,
        vec![
            Output::Degraded { since: 5 },
            Output::Ack {
                seq: 1,
                basis: AckBasis::Degraded
            },
            Output::Ack {
                seq: 2,
                basis: AckBasis::Degraded
            },
        ]
    );
}

#[test]
fn degrade_breach_persists_until_the_backlog_ships_then_clears_exactly_once() {
    let mut g = remote(OnUnreachable::Degrade);
    g.handle(0, Event::StoreReachable(false));
    commit(&mut g, 1, 1);
    commit(&mut g, 2, 2);
    g.handle(61_001, Event::Tick);
    assert!(g.rpo_breach().is_some());
    // The store returns but nothing has shipped yet: the alarm stays.
    assert_eq!(
        g.handle(62_000, Event::StoreReachable(true)),
        vec![Output::StoreRecovered]
    );
    assert!(g.rpo_breach().is_some());
    // A partial ship moves the unshipped marker but does not clear.
    let out = chunk(&mut g, 63_000, 1, 1);
    assert!(!out.contains(&Output::RpoBreachCleared));
    assert_eq!(g.rpo_breach().map(|b| b.unshipped_from_seq), Some(2));
    // The rest ships: cleared exactly once.
    let out = chunk(&mut g, 64_000, 2, 2);
    assert_eq!(
        out.iter()
            .filter(|o| **o == Output::RpoBreachCleared)
            .count(),
        1
    );
    assert!(g.rpo_breach().is_none());
    assert!(chunk(&mut g, 65_000, 3, 2).is_empty());
    assert!(g.handle(900_000, Event::Tick).is_empty());
}

#[test]
fn degrade_short_outage_that_ships_within_the_grace_never_breaches() {
    let mut g = remote(OnUnreachable::Degrade);
    g.handle(0, Event::StoreReachable(false));
    commit(&mut g, 1_000, 1);
    g.handle(5_000, Event::StoreReachable(true));
    let out = chunk(&mut g, 6_000, 1, 1);
    assert!(
        out.iter()
            .all(|o| !matches!(o, Output::RpoBreachRaised(_) | Output::RpoBreachCleared))
    );
    assert!(g.handle(500_000, Event::Tick).is_empty());
}

#[test]
fn ack_lost_after_local_commit_reports_unknown_and_the_retry_is_a_query() {
    let mut g = remote(OnUnreachable::Halt);
    commit(&mut g, 0, 7);
    assert_eq!(
        g.admit("c7"),
        Admission::Existing(CommandOutcome::Pending { seq: 7 })
    );
    let out = g.handle(30_000, Event::Tick);
    assert_eq!(out, vec![Output::CommitOutcomeUnknown { seq: 7 }]);
    // The retry of the same command id is a query, never a second commit.
    assert_eq!(
        g.admit("c7"),
        Admission::Existing(CommandOutcome::Unknown { seq: 7 })
    );
    assert_eq!(g.outcome("c7"), Some(CommandOutcome::Unknown { seq: 7 }));
    // The same command id cannot be registered again.
    assert!(commit(&mut g, 30_001, 8).len() == 1);
    assert!(
        g.handle(
            30_002,
            Event::LocalCommitDurable {
                seq: 9,
                command_id: "c7".into()
            }
        )
        .is_empty()
    );
    // The late chunk resolves it: the outcome becomes committed, with no second ack.
    let out = chunk(&mut g, 40_000, 1, 8);
    assert!(!acks(&out).contains(&7));
    assert_eq!(g.outcome("c7"), Some(CommandOutcome::Committed { seq: 7 }));
    assert_eq!(
        g.admit("c7"),
        Admission::Existing(CommandOutcome::Committed { seq: 7 })
    );
}

#[test]
fn a_retry_of_a_known_command_is_answered_even_while_halted() {
    let mut g = remote(OnUnreachable::Halt);
    commit(&mut g, 0, 1);
    g.handle(1, Event::StoreReachable(false));
    assert_eq!(
        g.admit("c1"),
        Admission::Existing(CommandOutcome::Pending { seq: 1 })
    );
    assert_eq!(g.admit("fresh"), Admission::Refuse(Refusal::Halted));
}

#[test]
fn flapping_store_acks_each_commit_exactly_once_in_order() {
    let mut g = remote(OnUnreachable::Degrade);
    let mut all = Vec::new();
    let mut seq = 0;
    for round in 0..6u64 {
        let t = round * 1_000;
        g.handle(t, Event::StoreReachable(false));
        seq += 1;
        all.extend(acks(&commit(&mut g, t + 1, seq)));
        g.handle(t + 2, Event::StoreReachable(true));
        seq += 1;
        all.extend(acks(&commit(&mut g, t + 3, seq)));
        all.extend(acks(&chunk(&mut g, t + 4, round, seq)));
    }
    assert_eq!(all, (1..=seq).collect::<Vec<_>>());
    assert!(g.rpo_breach().is_none());
    assert_eq!(g.pending_count(), 0);
}

#[test]
fn outcome_memory_is_bounded() {
    let mut g = AckGate::new(
        tenant(Policy::Local, true),
        GateConfig {
            retain_outcomes: 3,
            ..cfg()
        },
    );
    for seq in 1..=10 {
        commit(&mut g, seq, seq);
    }
    assert_eq!(g.outcome("c1"), None);
    assert!(g.outcome("c10").is_some());
}

#[test]
fn defaults_validation_and_error_codes() {
    assert_eq!(
        TenantDurability::default_for(true, false).policy,
        Policy::ReplicaSync
    );
    assert_eq!(
        TenantDurability::default_for(false, true).policy,
        Policy::RemoteSync {
            on_unreachable: OnUnreachable::Degrade
        }
    );
    assert_eq!(
        TenantDurability::default_for(false, false).policy,
        Policy::Local
    );
    assert!(
        tenant(
            Policy::RemoteSync {
                on_unreachable: OnUnreachable::Halt
            },
            false
        )
        .validate()
        .is_err()
    );
    assert!(tenant(Policy::Local, false).validate().is_ok());
    assert_eq!(DurabilityError::Halted.code(), "journal_durability.halted");
    assert_eq!(
        DurabilityError::OutcomeUnknown { seq: 1 }.code(),
        "journal_durability.outcome_unknown"
    );
    assert_eq!(
        DurabilityError::RpoBreach {
            since: 0,
            unshipped_from_seq: 1
        }
        .code(),
        "journal_durability.rpo_breach"
    );
}
