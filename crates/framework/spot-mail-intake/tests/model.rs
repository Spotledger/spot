//! Property: for any interleaving of crashes and polls, every message reaches the sink exactly
//! once, and the cursor never moves backwards.

mod support;

use proptest::prelude::*;
use spot_mail_intake::{CursorStore, PollState, poll_once};
use support::{Harness, account, message};

#[derive(Debug, Clone, Copy)]
enum Action {
    /// A poll in which the cursor write fails.
    CrashOnStore,
    /// A poll in which the sink is unavailable for the message at this offset.
    SinkDown(u32),
    /// A plain poll.
    Poll,
    /// New mail arrives.
    Arrive(u8),
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        Just(Action::CrashOnStore),
        (1u32..8).prop_map(Action::SinkDown),
        Just(Action::Poll),
        (1u8..4).prop_map(Action::Arrive),
    ]
}

async fn poll(h: &Harness, acct: &spot_mail_intake::Account, state: &mut PollState) {
    let deps = h.deps("node-a");
    poll_once(acct, h.now(), state, &deps).await;
}

async fn run(actions: Vec<Action>) -> Result<(), String> {
    let h = Harness::new().await;
    let mut acct = account();
    acct.limits.max_messages_per_poll = 3;
    let mut state = PollState::new();
    let mut next_uid = 1u32;
    let mut last_cursor = 0u32;

    for step in actions {
        match step {
            Action::Arrive(n) => {
                for _ in 0..n {
                    h.mailbox.push(message(next_uid, &format!("m{next_uid}@x")));
                    next_uid += 1;
                }
            }
            Action::CrashOnStore => {
                h.cursors.fail_next_stores(1);
                poll(&h, &acct, &mut state).await;
                h.cursors.fail_next_stores(0);
            }
            Action::SinkDown(offset) => {
                h.sink.retry_on(offset);
                poll(&h, &acct, &mut state).await;
                h.sink.clear_retry();
            }
            Action::Poll => {
                poll(&h, &acct, &mut state).await;
            }
        }
        // Retried failures must not stack up into a halt; the account stays pollable.
        if state.halted().is_some() {
            return Err("the account halted".to_owned());
        }
        let stored = match h.cursors.load("support").await {
            Ok(cursor) => cursor.map_or(0, |cursor| cursor.last_uid),
            Err(error) => return Err(format!("load: {error}")),
        };
        if stored < last_cursor {
            return Err(format!("cursor moved back from {last_cursor} to {stored}"));
        }
        last_cursor = stored;
    }

    // Drain: plain polls until nothing is left.
    for _ in 0..(next_uid + 2) {
        poll(&h, &acct, &mut state).await;
    }
    let mut created = h.sink.created_uids();
    created.sort_unstable();
    let expected: Vec<u32> = (1..next_uid).collect();
    if created == expected {
        Ok(())
    } else {
        Err(format!("sink saw {created:?}, expected {expected:?}"))
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn every_message_is_accepted_exactly_once(actions in proptest::collection::vec(action(), 1..40)) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build();
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(error) => return Err(TestCaseError::fail(format!("runtime: {error}"))),
        };
        if let Err(message) = runtime.block_on(run(actions)) {
            return Err(TestCaseError::fail(message));
        }
    }
}
