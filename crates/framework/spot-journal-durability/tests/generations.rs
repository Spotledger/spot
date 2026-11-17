//! The gate keys on `(generation, seq)` (WP-JRN-04.T5): the `seq` restart of a new generation
//! after a fork is acknowledged under every policy, a stale writer is refused with a typed error,
//! and a failed local commit withdraws only its own position.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_journal_durability::*;

fn p(generation: Generation, seq: Seq) -> Position {
    Position::new(generation, seq)
}

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

fn all_policies() -> Vec<(&'static str, Policy)> {
    vec![
        ("local", Policy::Local),
        ("replica-sync", Policy::ReplicaSync),
        (
            "remote-sync degrade",
            Policy::RemoteSync {
                on_unreachable: OnUnreachable::Degrade,
            },
        ),
        (
            "remote-sync halt",
            Policy::RemoteSync {
                on_unreachable: OnUnreachable::Halt,
            },
        ),
    ]
}

fn remote() -> AckGate {
    gate(Policy::RemoteSync {
        on_unreachable: OnUnreachable::Halt,
    })
}

fn commit(g: &mut AckGate, now: u64, position: Position, id: &str) -> Vec<Output> {
    g.handle(
        now,
        Event::LocalCommitDurable {
            position,
            command_id: id.into(),
        },
    )
}

fn fail(g: &mut AckGate, now: u64, position: Position, id: &str) -> Vec<Output> {
    g.handle(
        now,
        Event::LocalCommitFailed {
            position,
            command_id: id.into(),
        },
    )
}

fn chunk(g: &mut AckGate, now: u64, up_to: Position) -> Vec<Output> {
    g.handle(
        now,
        Event::ChunkDurable {
            batch: BatchId(now),
            up_to,
        },
    )
}

fn standby(g: &mut AckGate, now: u64, position: Position) -> Vec<Output> {
    g.handle(now, Event::StandbyConfirmed { position })
}

fn acked(out: &[Output]) -> Vec<Position> {
    out.iter()
        .filter_map(|o| match o {
            Output::Ack { position, .. } => Some(*position),
            _ => None,
        })
        .collect()
}

/// Commit `1.1..1.4`, release them, then the first commit of generation 2 under `policy`.
fn run_fork(policy: Policy) -> (AckGate, Vec<Output>) {
    let mut g = gate(policy);
    for seq in 1..=4 {
        commit(&mut g, seq, p(1, seq), &format!("a{seq}"));
    }
    standby(&mut g, 10, p(1, 4));
    chunk(&mut g, 11, p(1, 4));
    let out = commit(&mut g, 20, p(2, 1), "b1");
    (g, out)
}

#[test]
fn position_orders_generation_first() {
    assert!(p(1, 9) < p(2, 1));
    assert!(p(2, 1) < p(2, 2));
    assert!(p(3, 0) > p(2, u64::MAX));
    assert_eq!(p(2, 7).to_string(), "2.7");
    assert_eq!(
        Position::new(2, 7),
        Position {
            generation: 2,
            seq: 7
        }
    );
}

#[test]
fn the_first_commit_of_a_new_generation_is_acknowledged_under_every_policy() {
    for (name, policy) in all_policies() {
        let (mut g, out) = run_fork(policy);
        match policy {
            Policy::Local => assert_eq!(
                out,
                vec![Output::Ack {
                    position: p(2, 1),
                    basis: AckBasis::Local
                }],
                "{name}"
            ),
            Policy::ReplicaSync => {
                // The old generation's confirmation does not cover the new one.
                assert_eq!(out, vec![Output::Wait { position: p(2, 1) }], "{name}");
                let out = standby(&mut g, 21, p(2, 1));
                assert_eq!(
                    out,
                    vec![Output::Ack {
                        position: p(2, 1),
                        basis: AckBasis::Standby
                    }],
                    "{name}"
                );
            }
            Policy::RemoteSync { .. } => {
                assert_eq!(out, vec![Output::Wait { position: p(2, 1) }], "{name}");
                let out = chunk(&mut g, 21, p(2, 1));
                assert_eq!(
                    out,
                    vec![Output::Ack {
                        position: p(2, 1),
                        basis: AckBasis::Chunk
                    }],
                    "{name}"
                );
            }
        }
        assert_eq!(
            g.outcome("b1"),
            Some(CommandOutcome::Committed { position: p(2, 1) }),
            "{name}"
        );
        assert_eq!(g.pending_count(), 0, "{name}");
        assert_eq!(g.rejected_events(), 0, "{name}");
    }
}

#[test]
fn an_old_generation_chunk_never_acknowledges_a_new_generation_commit() {
    let mut g = remote();
    commit(&mut g, 0, p(1, 1), "a");
    commit(&mut g, 1, p(2, 1), "b");
    // A chunk up to 1.9 covers generation 1 (including 1.1) but not 2.1, although 1 <= 9.
    assert_eq!(acked(&chunk(&mut g, 2, p(1, 9))), vec![p(1, 1)]);
    assert_eq!(g.pending_count(), 1);
    assert_eq!(
        g.outcome("b"),
        Some(CommandOutcome::Pending { position: p(2, 1) })
    );
    assert_eq!(acked(&chunk(&mut g, 3, p(2, 1))), vec![p(2, 1)]);
}

#[test]
fn a_cumulative_confirmation_of_a_new_generation_covers_the_earlier_one() {
    let mut g = gate(Policy::ReplicaSync);
    commit(&mut g, 0, p(1, 3), "a");
    commit(&mut g, 1, p(2, 1), "b");
    assert_eq!(acked(&standby(&mut g, 2, p(2, 1))), vec![p(1, 3), p(2, 1)]);
}

#[test]
fn a_commit_of_a_lower_generation_is_refused_with_a_typed_error() {
    for (name, policy) in all_policies() {
        let (mut g, _) = run_fork(policy);
        let pending = g.pending_count();
        let out = commit(&mut g, 30, p(1, 5), "late");
        let error = DurabilityError::StaleGeneration {
            position: p(1, 5),
            current: 2,
        };
        assert_eq!(
            out,
            vec![Output::Rejected {
                position: p(1, 5),
                error: error.clone()
            }],
            "{name}"
        );
        assert_eq!(error.code(), "journal_durability.stale_generation");
        // The refusal changed nothing: no entry, no pending, no acknowledgement.
        assert_eq!(g.outcome("late"), None, "{name}");
        assert_eq!(g.pending_count(), pending, "{name}");
        assert_eq!(g.admit("late"), Admission::Proceed, "{name}");
        // The gate still accepts the next commit of the current generation.
        let out = commit(&mut g, 31, p(2, 2), "b2");
        assert!(
            matches!(out.as_slice(), [Output::Ack { .. } | Output::Wait { .. }]),
            "{name}: {out:?}"
        );
    }
}

#[test]
fn a_stale_commit_is_refused_even_when_its_seq_is_above_the_current_one() {
    let mut g = gate(Policy::Local);
    commit(&mut g, 0, p(2, 1), "b");
    let out = commit(&mut g, 1, p(1, 500), "late");
    assert_eq!(
        out,
        vec![Output::Rejected {
            position: p(1, 500),
            error: DurabilityError::StaleGeneration {
                position: p(1, 500),
                current: 2
            }
        }]
    );
}

#[test]
fn a_repeated_or_lower_position_in_the_current_generation_is_a_typed_refusal() {
    let mut g = gate(Policy::Local);
    commit(&mut g, 0, p(2, 1), "a");
    commit(&mut g, 1, p(2, 2), "b");
    for (position, id) in [(p(2, 2), "c"), (p(2, 1), "d")] {
        let out = commit(&mut g, 2, position, id);
        assert_eq!(
            out,
            vec![Output::Rejected {
                position,
                error: DurabilityError::OutOfOrder {
                    position,
                    last: p(2, 2)
                }
            }]
        );
        assert_eq!(g.outcome(id), None);
    }
    assert_eq!(
        DurabilityError::OutOfOrder {
            position: p(2, 1),
            last: p(2, 2)
        }
        .code(),
        "journal_durability.out_of_order"
    );
}

#[test]
fn a_reused_command_id_is_a_typed_refusal_not_a_silent_drop() {
    let mut g = remote();
    commit(&mut g, 0, p(1, 1), "a");
    let out = commit(&mut g, 1, p(2, 1), "a");
    assert_eq!(
        out,
        vec![Output::Rejected {
            position: p(2, 1),
            error: DurabilityError::DuplicateCommand { position: p(2, 1) }
        }]
    );
    assert_eq!(
        DurabilityError::DuplicateCommand { position: p(2, 1) }.code(),
        "journal_durability.duplicate_command"
    );
    // Nothing moved: the pending entry is the original one.
    assert_eq!(g.pending_count(), 1);
    assert_eq!(
        g.outcome("a"),
        Some(CommandOutcome::Pending { position: p(1, 1) })
    );
}

#[test]
fn local_commit_failed_withdraws_only_that_entry() {
    let mut g = remote();
    commit(&mut g, 0, p(1, 1), "a");
    commit(&mut g, 1, p(1, 2), "b");
    commit(&mut g, 2, p(2, 1), "c");
    commit(&mut g, 3, p(2, 2), "d");
    assert_eq!(
        fail(&mut g, 4, p(2, 1), "c"),
        vec![Output::Withdrawn { position: p(2, 1) }]
    );
    assert_eq!(g.pending_count(), 3);
    assert_eq!(
        g.outcome("c"),
        Some(CommandOutcome::Withdrawn { position: p(2, 1) })
    );
    for (id, position) in [("a", p(1, 1)), ("b", p(1, 2)), ("d", p(2, 2))] {
        assert_eq!(
            g.outcome(id),
            Some(CommandOutcome::Pending { position }),
            "{id}"
        );
    }
    // The chunk acknowledges the three others and nothing for the withdrawn one.
    let out = chunk(&mut g, 5, p(2, 2));
    assert_eq!(acked(&out), vec![p(1, 1), p(1, 2), p(2, 2)]);
    assert_eq!(g.rejected_events(), 0);
}

#[test]
fn local_commit_failed_does_not_cross_generations_with_the_same_seq() {
    let mut g = remote();
    commit(&mut g, 0, p(1, 1), "a");
    // 2.1 was never seen: the failure of "2.1" must not withdraw 1.1, even with the same id.
    assert!(fail(&mut g, 1, p(2, 1), "a").is_empty());
    assert_eq!(g.rejected_events(), 1);
    assert_eq!(g.pending_count(), 1);
    assert_eq!(
        g.outcome("a"),
        Some(CommandOutcome::Pending { position: p(1, 1) })
    );
    // And the right position still works.
    assert_eq!(
        fail(&mut g, 2, p(1, 1), "a"),
        vec![Output::Withdrawn { position: p(1, 1) }]
    );
}

#[test]
fn a_withdrawn_command_retries_in_the_next_generation() {
    let mut g = remote();
    commit(&mut g, 0, p(1, 2), "a");
    fail(&mut g, 1, p(1, 2), "a");
    assert_eq!(g.admit("a"), Admission::Proceed);
    assert_eq!(
        commit(&mut g, 2, p(2, 1), "a"),
        vec![Output::Wait { position: p(2, 1) }]
    );
    assert_eq!(
        g.admit("a"),
        Admission::Existing(CommandOutcome::Pending { position: p(2, 1) })
    );
    assert_eq!(acked(&chunk(&mut g, 3, p(2, 1))), vec![p(2, 1)]);
    assert_eq!(
        g.outcome("a"),
        Some(CommandOutcome::Committed { position: p(2, 1) })
    );
}

#[test]
fn the_rpo_breach_reports_the_first_unshipped_position_across_generations() {
    let mut g = gate(Policy::RemoteSync {
        on_unreachable: OnUnreachable::Degrade,
    });
    g.handle(0, Event::StoreReachable(false));
    commit(&mut g, 1, p(1, 1), "a");
    commit(&mut g, 2, p(2, 1), "b");
    let out = g.handle(61_001, Event::Tick);
    assert_eq!(
        out,
        vec![Output::RpoBreachRaised(RpoBreach {
            since: 1,
            unshipped_from: p(1, 1)
        })]
    );
    // Shipping generation 1 leaves 2.1 as the first unshipped position.
    chunk(&mut g, 62_000, p(1, 1));
    assert_eq!(g.rpo_breach().map(|b| b.unshipped_from), Some(p(2, 1)));
    let out = chunk(&mut g, 63_000, p(2, 1));
    assert!(out.contains(&Output::RpoBreachCleared));
    assert!(g.rpo_breach().is_none());
}

#[test]
fn outcome_unknown_names_the_generation_and_the_new_error_codes_are_stable() {
    let mut g = remote();
    commit(&mut g, 0, p(2, 1), "a");
    let out = g.handle(30_000, Event::Tick);
    assert_eq!(
        out,
        vec![Output::CommitOutcomeUnknown { position: p(2, 1) }]
    );
    let err = DurabilityError::OutcomeUnknown { position: p(2, 1) };
    assert_eq!(err.code(), "journal_durability.outcome_unknown");
    assert!(err.to_string().contains("2.1"));
}
