//! Property run: for any interleaving of events no seq is acknowledged before its policy's
//! condition holds (WP-JRN-04.T1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_journal_durability::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
enum Step {
    Commit,
    Standby(u64),
    Chunk(u64),
    Fail,
    Reach(bool),
    Tick,
    FailCommit(u64),
}

fn step() -> impl Strategy<Value = (u64, Step)> {
    (
        0u64..40_000,
        prop_oneof![
            4 => Just(Step::Commit),
            2 => (0u64..30).prop_map(Step::Standby),
            3 => (0u64..30).prop_map(Step::Chunk),
            1 => Just(Step::Fail),
            2 => any::<bool>().prop_map(Step::Reach),
            2 => Just(Step::Tick),
            2 => (1u64..40).prop_map(Step::FailCommit),
        ],
    )
}

fn policy() -> impl Strategy<Value = Policy> {
    prop_oneof![
        Just(Policy::Local),
        Just(Policy::ReplicaSync),
        Just(Policy::RemoteSync {
            on_unreachable: OnUnreachable::Degrade
        }),
        Just(Policy::RemoteSync {
            on_unreachable: OnUnreachable::Halt
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn nothing_is_acknowledged_before_its_policy_condition(
        policy in policy(),
        steps in proptest::collection::vec(step(), 1..80),
    ) {
        let mut gate = AckGate::new(
            TenantDurability { policy, journal_target_configured: true },
            GateConfig { rpo_grace_millis: 5_000, ack_deadline_millis: 10_000, retain_outcomes: 1_000 },
        );
        let mut now = 0u64;
        let mut next_seq = 0u64;
        let mut committed: BTreeMap<u64, ()> = BTreeMap::new();
        let mut standby: Option<u64> = None;
        let mut shipped: Option<u64> = None;
        let mut reachable = true;
        let mut acked: Vec<u64> = Vec::new();
        let mut withdrawn: Vec<u64> = Vec::new();
        let mut outputs_per_seq: BTreeMap<u64, u32> = BTreeMap::new();
        let mut breaches = 0i32;
        let mut clears = 0i32;

        for (dt, s) in steps {
            now += dt;
            let event = match s {
                Step::Commit => {
                    next_seq += 1;
                    committed.insert(next_seq, ());
                    Event::LocalCommitDurable { seq: next_seq, command_id: format!("c{next_seq}") }
                }
                Step::Standby(n) => {
                    standby = standby.max(Some(n));
                    Event::StandbyConfirmed { seq: n }
                }
                Step::Chunk(n) => {
                    // A chunk proves reachability.
                    if matches!(policy, Policy::RemoteSync { .. }) {
                        reachable = true;
                        shipped = shipped.max(Some(n));
                    }
                    Event::ChunkDurable { batch: BatchId(n), up_to_seq: n }
                }
                Step::Fail => {
                    reachable = false;
                    Event::ChunkFailed { batch: BatchId(0), reason: FailureReason::Unreachable }
                }
                Step::Reach(up) => {
                    reachable = up;
                    Event::StoreReachable(up)
                }
                Step::Tick => Event::Tick,
                Step::FailCommit(n) => Event::LocalCommitFailed {
                    seq: n,
                    command_id: format!("c{n}"),
                },
            };
            for out in gate.handle(now, event) {
                match out {
                    Output::Ack { seq, basis } => {
                        prop_assert!(committed.contains_key(&seq), "ack before local commit");
                        match policy {
                            Policy::Local => prop_assert_eq!(basis, AckBasis::Local),
                            Policy::ReplicaSync => {
                                prop_assert_eq!(basis, AckBasis::Standby);
                                prop_assert!(standby.is_some_and(|s| seq <= s), "ack before standby");
                            }
                            Policy::RemoteSync { on_unreachable } => match basis {
                                AckBasis::Chunk => prop_assert!(
                                    shipped.is_some_and(|s| seq <= s),
                                    "remote-sync ack not covered by a ChunkDurable"
                                ),
                                AckBasis::Degraded => {
                                    prop_assert_eq!(on_unreachable, OnUnreachable::Degrade);
                                    prop_assert!(!reachable, "degraded ack while store reachable");
                                }
                                _ => prop_assert!(false, "wrong basis"),
                            },
                        }
                        prop_assert!(!acked.contains(&seq), "acked twice");
                        if let Some(last) = acked.last() {
                            prop_assert!(seq > *last, "acks out of order");
                        }
                        prop_assert!(!withdrawn.contains(&seq), "ack for a withdrawn seq");
                        prop_assert!(!acked.contains(&seq));
                        acked.push(seq);
                        *outputs_per_seq.entry(seq).or_insert(0) += 1;
                    }
                    Output::Withdrawn { seq } => {
                        prop_assert!(!acked.contains(&seq), "withdrew an acknowledged seq");
                        prop_assert!(!withdrawn.contains(&seq), "withdrawn twice");
                        withdrawn.push(seq);
                        *outputs_per_seq.entry(seq).or_insert(0) += 1;
                    }
                    Output::RpoBreachRaised(_) => breaches += 1,
                    Output::RpoBreachCleared => clears += 1,
                    _ => {}
                }
            }
            // One terminal output per sequence number (Ack or Withdrawn).
            prop_assert!(outputs_per_seq.values().all(|n| *n == 1), "two outputs for one seq");
            // Halt never lets a new command in while the store is unreachable.
            if matches!(policy, Policy::RemoteSync { on_unreachable: OnUnreachable::Halt }) && !reachable {
                prop_assert_eq!(gate.admit("brand-new"), Admission::Refuse(Refusal::Halted));
            }
            // Raised and cleared strictly alternate.
            prop_assert!(clears <= breaches && breaches - clears <= 1);
            prop_assert_eq!(gate.rpo_breach().is_some(), breaches - clears == 1);
        }
    }
}
