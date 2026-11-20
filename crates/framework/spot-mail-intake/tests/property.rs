//! For any interleaving of cursor-store crashes and polls, an idempotent-on-key sink and the
//! seen store together accept every message exactly once.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;

use proptest::prelude::*;
use spot_mail_intake::{PollState, poll_once};
use support::{Harness, account, message};

#[derive(Debug, Clone)]
enum Step {
    /// New mail arrives (this many messages).
    Arrive(u8),
    /// The next cursor write fails, then a poll runs.
    CrashPoll,
    /// A plain poll.
    Poll,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (1u8..4).prop_map(Step::Arrive),
        Just(Step::CrashPoll),
        Just(Step::Poll),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn every_message_reaches_the_sink_exactly_once(steps in proptest::collection::vec(step(), 1..24)) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let mut h = Harness::new().await;
            h.sink.idempotent = true;
            let mut state = PollState::new();
            let mut next_uid = 1u32;
            for step in &steps {
                match step {
                    Step::Arrive(n) => {
                        for _ in 0..*n {
                            h.mailbox.push(message(next_uid, &format!("<m{next_uid}@x>")));
                            next_uid += 1;
                        }
                    }
                    Step::CrashPoll => {
                        h.cursors.fail_next_stores(1);
                        let _ = poll_once(&account(), h.now(), &mut state, &h.deps("node-a")).await;
                    }
                    Step::Poll => {
                        let _ = poll_once(&account(), h.now(), &mut state, &h.deps("node-a")).await;
                    }
                }
            }
            // Drain: enough clean polls to take everything in.
            for _ in 0..40 {
                let _ = poll_once(&account(), h.now(), &mut state, &h.deps("node-a")).await;
            }
            let created = h.sink.created_uids();
            let unique: BTreeSet<u32> = created.iter().copied().collect();
            prop_assert_eq!(created.len(), unique.len(), "no message created twice");
            let expected: BTreeSet<u32> = (1..next_uid).collect();
            prop_assert_eq!(unique, expected, "every message created");
            Ok(())
        })?;
    }
}
