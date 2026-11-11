//! Property: no ticket resolves `Acked` before its policy condition holds.

use proptest::prelude::*;
use spot_journal_durability::Policy;

use super::support::{REMOTE_DEGRADE, REMOTE_HALT, rig};
use crate::CommitAnswer;

#[derive(Debug, Clone)]
enum Op {
    Submit,
    Confirm,
    Standby,
    Step(u64),
    StoreDown(bool),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Submit),
        3 => Just(Op::Confirm),
        1 => Just(Op::Standby),
        3 => (0u64..2_000).prop_map(Op::Step),
        1 => any::<bool>().prop_map(Op::StoreDown),
    ]
}

fn policy() -> impl Strategy<Value = Policy> {
    prop_oneof![
        Just(Policy::Local),
        Just(Policy::ReplicaSync),
        Just(REMOTE_DEGRADE),
        Just(REMOTE_HALT),
    ]
}

proptest! {
    #[test]
    fn nothing_is_acked_before_its_condition(policy in policy(), ops in prop::collection::vec(op(), 1..60)) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let rig = rig(policy);
            let mut submitted = 0u64;
            let mut confirmed = 0u64;
            let mut standby = 0u64;
            let mut was_down = false;
            for op in ops {
                match op {
                    Op::Submit => {
                        let seq = submitted + 1;
                        if let Ok(mut ticket) = rig.driver.submit(super::support::request(seq)) {
                            if matches!(ticket.try_answer(), Some(CommitAnswer::Refused(_))) {
                                continue;
                            }
                            submitted = seq;
                        }
                    }
                    Op::Confirm => {
                        if confirmed < submitted {
                            rig.driver.local_commit_durable(confirmed + 1).unwrap();
                            confirmed += 1;
                        }
                    }
                    Op::Standby => {
                        standby = confirmed;
                        rig.driver.standby_confirmed(standby);
                    }
                    Op::Step(ms) => {
                        rig.clock.0.advance(ms);
                        rig.driver.step().await;
                    }
                    Op::StoreDown(down) => {
                        rig.relay.state().down = down;
                        was_down |= down;
                    }
                }
                let held = rig.relay.state().held.clone();
                for seq in rig.recorder.acked() {
                    prop_assert!(seq <= confirmed, "acked {seq} before its local commit");
                    match policy {
                        Policy::ReplicaSync => prop_assert!(seq <= standby),
                        Policy::RemoteSync { on_unreachable } if on_unreachable == spot_journal_durability::OnUnreachable::Halt || !was_down => {
                            prop_assert!(held.contains(&seq), "acked {seq} before its chunk");
                        }
                        _ => {}
                    }
                }
            }
            let acked = rig.recorder.acked();
            let mut sorted = acked.clone();
            sorted.sort_unstable();
            prop_assert_eq!(acked, sorted, "released out of order");
            Ok(())
        })?;
    }
}
