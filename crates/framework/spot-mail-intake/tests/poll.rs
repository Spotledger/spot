//! `poll_once` against the scripted mailbox and the in-memory ports.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use spot_coordinator::{Coordinator, FencingToken};
use spot_mail::{MailError, MailboxCursor, PermanentFailure, TransientFailure};
use spot_mail_intake::{
    AuditEvent, CursorStore, MAX_BACKOFF, PollOutcome, PollState, backoff, poll_once,
};
use support::{Harness, OWN_ADDRESS, PASSWORD, SITE, account, dropped, message, must};

async fn poll(h: &Harness, state: &mut PollState) -> spot_mail_intake::PollReport {
    poll_once(&account(), h.now(), state, &h.deps("node-a")).await
}

#[tokio::test]
async fn first_poll_imports_in_uid_order_and_the_second_only_new_mail() {
    let h = Harness::new().await;
    h.mailbox.push(message(3, "<c@x>"));
    h.mailbox.push(message(1, "<a@x>"));
    h.mailbox.push(message(2, "<b@x>"));
    let mut state = PollState::new();

    let first = poll(&h, &mut state).await;
    assert_eq!(first.outcome, PollOutcome::Polled);
    assert_eq!(first.accepted, 3);
    assert_eq!(first.uid_range, Some((1, 3)));
    assert_eq!(h.sink.created_uids(), vec![1, 2, 3]);
    assert_eq!(first.next_delay, Some(Duration::from_secs(60)));

    h.mailbox.push(message(4, "<d@x>"));
    let second = poll(&h, &mut state).await;
    assert_eq!(second.accepted, 1);
    assert_eq!(h.sink.created_uids(), vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn a_crash_between_accept_and_cursor_redelivers_once() {
    let h = Harness::new().await;
    h.mailbox.push(message(1, "<a@x>"));
    h.mailbox.push(message(2, "<b@x>"));
    let mut state = PollState::new();

    h.cursors.fail_next_stores(1);
    let crashed = poll(&h, &mut state).await;
    assert_eq!(crashed.outcome, PollOutcome::Failed);
    assert_eq!(crashed.reason, Some("store_unavailable"));
    assert_eq!(h.cursors.load("support").await, Ok(None));

    let again = poll(&h, &mut state).await;
    assert_eq!(again.outcome, PollOutcome::Polled);
    assert_eq!(again.accepted, 0, "the seen store hides what the sink took");
    assert_eq!(again.duplicates, 2);
    assert_eq!(
        h.sink.created_uids(),
        vec![1, 2],
        "the sink saw each message once"
    );
    assert_eq!(
        h.cursors.load("support").await,
        Ok(Some(MailboxCursor {
            uid_validity: 7,
            last_uid: 2
        }))
    );
}

#[tokio::test]
async fn a_poll_without_the_lease_does_nothing() {
    let h = Harness::new().await;
    h.mailbox.push(message(1, "<a@x>"));
    let name = format!("mail_intake:{SITE}:support");
    let held = must(
        h.coordinator
            .acquire(&name, "node-b", Duration::from_secs(300))
            .await,
    );
    let mut state = PollState::new();
    let report = poll(&h, &mut state).await;
    assert_eq!(report.outcome, PollOutcome::NotLeader);
    assert_eq!(h.mailbox.fetches(), 0);
    assert!(h.sink.calls().is_empty());
    must(h.coordinator.release(&held).await);

    let report = poll(&h, &mut state).await;
    assert_eq!(report.outcome, PollOutcome::Polled);
    assert_eq!(report.accepted, 1);
}

#[tokio::test]
async fn an_old_leader_cannot_move_the_cursor() {
    let h = Harness::new().await;
    let cursor = |last_uid| MailboxCursor {
        uid_validity: 7,
        last_uid,
    };
    must(h.cursors.store("support", cursor(5), FencingToken(2)).await);
    // The new leader (token 3) wrote; the old leader (token 2) is stale from now on.
    must(h.cursors.store("support", cursor(6), FencingToken(3)).await);
    assert!(
        h.cursors
            .store("support", cursor(99), FencingToken(2))
            .await
            .is_err()
    );
    assert_eq!(h.cursors.load("support").await, Ok(Some(cursor(6))));
    assert_eq!(h.cursors.fence_of("support"), Some(3));
}

#[tokio::test]
async fn a_poll_whose_token_is_stale_reports_not_leader_and_changes_nothing() {
    let h = Harness::new().await;
    // A previous leader with a higher token already stored a cursor.
    must(
        h.cursors
            .store(
                "support",
                MailboxCursor {
                    uid_validity: 7,
                    last_uid: 0,
                },
                FencingToken(u64::MAX),
            )
            .await,
    );
    h.mailbox.push(message(1, "<a@x>"));
    let mut state = PollState::new();
    let report = poll(&h, &mut state).await;
    assert_eq!(report.outcome, PollOutcome::NotLeader);
    assert_eq!(report.reason, Some("stale_fence"));
    assert_eq!(
        h.cursors.load("support").await,
        Ok(Some(MailboxCursor {
            uid_validity: 7,
            last_uid: 0
        }))
    );
}

#[tokio::test]
async fn auto_replies_bulk_and_own_mail_are_dropped_and_counted() {
    let h = Harness::new().await;
    let mut auto = message(1, "<auto@x>");
    auto.auto_submitted = true;
    let mut own = message(2, "<own@x>");
    own.from.as_mut().unwrap().address = OWN_ADDRESS.to_uppercase();
    h.mailbox.push(auto);
    h.mailbox.push(own);
    h.mailbox.push(message(3, "<real@x>"));
    let mut state = PollState::new();
    let report = poll(&h, &mut state).await;
    assert_eq!(report.accepted, 1);
    assert_eq!(dropped(&report).get("auto_reply"), Some(&1));
    assert_eq!(dropped(&report).get("own_mail"), Some(&1));
    assert_eq!(
        h.sink.calls(),
        vec![3],
        "dropped mail never reaches the sink"
    );
    assert_eq!(
        h.cursors.load("support").await,
        Ok(Some(MailboxCursor {
            uid_validity: 7,
            last_uid: 3
        }))
    );
}

#[tokio::test]
async fn a_poison_message_does_not_stop_the_account() {
    let h = Harness::new().await;
    h.mailbox.push(message(1, "<a@x>"));
    h.mailbox.push(message(2, "<b@x>"));
    h.mailbox.push(message(3, "<c@x>"));
    h.sink.reject_on(2);
    let mut state = PollState::new();
    let report = poll(&h, &mut state).await;
    assert_eq!(report.outcome, PollOutcome::Polled);
    assert_eq!(report.rejected, 1);
    assert_eq!(report.accepted, 2);
    assert_eq!(h.sink.created_uids(), vec![1, 3]);
    assert!(
        h.audit
            .entries()
            .iter()
            .any(|(_, e)| matches!(e, AuditEvent::SinkRejected { uid: 2, .. }))
    );

    let again = poll(&h, &mut state).await;
    assert_eq!(again.fetched, 0, "the poison message is not fetched again");
}

#[tokio::test]
async fn a_retryable_error_stops_the_batch_exactly_at_that_message() {
    let h = Harness::new().await;
    for uid in 1..=4 {
        h.mailbox.push(message(uid, &format!("<m{uid}@x>")));
    }
    h.sink.retry_on(3);
    let mut state = PollState::new();
    let report = poll(&h, &mut state).await;
    assert_eq!(report.outcome, PollOutcome::Failed);
    assert_eq!(report.reason, Some("sink_retryable"));
    assert_eq!(report.accepted, 2);
    assert_eq!(report.next_delay, Some(backoff(1)));
    assert_eq!(h.sink.calls(), vec![1, 2, 3], "message 4 was not offered");
    assert_eq!(
        h.cursors.load("support").await,
        Ok(Some(MailboxCursor {
            uid_validity: 7,
            last_uid: 2
        }))
    );

    h.sink.clear_retry();
    let resumed = poll(&h, &mut state).await;
    assert_eq!(resumed.outcome, PollOutcome::Polled);
    assert_eq!(h.sink.created_uids(), vec![1, 2, 3, 4]);
    assert_eq!(state.failures(), 0);
}

#[tokio::test]
async fn the_daily_cap_drops_and_still_advances_with_one_audit_entry() {
    let h = Harness::new().await;
    for uid in 1..=5 {
        h.mailbox.push(message(uid, &format!("<m{uid}@x>")));
    }
    let mut capped = account();
    capped.limits.max_messages_per_day = 2;
    let mut state = PollState::new();
    let deps = h.deps("node-a");
    let report = poll_once(&capped, h.now(), &mut state, &deps).await;
    assert_eq!(report.accepted, 2);
    assert_eq!(dropped(&report).get("daily_cap"), Some(&3));
    assert_eq!(
        h.cursors.load("support").await,
        Ok(Some(MailboxCursor {
            uid_validity: 7,
            last_uid: 5
        })),
        "a flood cannot wedge the account"
    );
    h.mailbox.push(message(6, "<m6@x>"));
    let again = poll_once(&capped, h.now(), &mut state, &deps).await;
    assert_eq!(dropped(&again).get("daily_cap"), Some(&1));
    let caps = h
        .audit
        .entries()
        .iter()
        .filter(|(_, e)| matches!(e, AuditEvent::DailyCapReached { .. }))
        .count();
    assert_eq!(caps, 1, "one entry per day");
}

#[tokio::test]
async fn a_mailbox_reset_stores_the_new_validity_and_imports_nothing() {
    let h = Harness::new().await;
    h.mailbox.push(message(1, "<a@x>"));
    let mut state = PollState::new();
    poll(&h, &mut state).await;

    h.mailbox.set_validity(8);
    let report = poll(&h, &mut state).await;
    assert_eq!(report.outcome, PollOutcome::MailboxReset);
    assert_eq!(report.accepted, 0);
    assert_eq!(h.sink.created_uids(), vec![1]);
    assert_eq!(
        h.cursors.load("support").await,
        Ok(Some(MailboxCursor {
            uid_validity: 8,
            last_uid: 0
        }))
    );
    assert!(h.audit.entries().iter().any(|(_, e)| matches!(
        e,
        AuditEvent::MailboxReset {
            old_validity: Some(7),
            new_validity: 8
        }
    )));

    // The re-scan that follows finds the same Message-ID and the seen store skips it.
    let rescan = poll(&h, &mut state).await;
    assert_eq!(rescan.accepted, 0);
    assert_eq!(rescan.duplicates, 1);
    assert_eq!(h.sink.created_uids(), vec![1]);
}

#[tokio::test]
async fn an_oversize_message_arrives_as_a_flagged_stub() {
    let h = Harness::new().await;
    let mut big = message(1, "<big@x>");
    big.raw_size = 5_000;
    h.mailbox.push(big);
    let mut state = PollState::new();
    let report = poll(&h, &mut state).await;
    assert_eq!(report.accepted, 1);
    assert_eq!(report.truncated, 1);
    assert_eq!(h.sink.truncated_seen(), vec![1]);
}

#[tokio::test]
async fn an_authentication_failure_halts_without_leaking_the_password() {
    let h = Harness::new().await;
    h.mailbox.push(message(1, "<a@x>"));
    h.mailbox.fail_with(Some(MailError::Permanent(
        PermanentFailure::Authentication { reply: None },
    )));
    let mut state = PollState::new();
    let report = poll(&h, &mut state).await;
    assert_eq!(report.outcome, PollOutcome::AuthFailed);
    assert_eq!(report.next_delay, None);
    assert_eq!(state.halted(), Some("auth_failed"));

    // Polling does not resume by itself, and the audit holds one entry.
    let fetches = h.mailbox.fetches();
    let second = poll(&h, &mut state).await;
    assert_eq!(second.outcome, PollOutcome::AuthFailed);
    assert_eq!(h.mailbox.fetches(), fetches);
    let audits = h.audit.entries();
    assert_eq!(
        audits
            .iter()
            .filter(|(_, e)| *e == AuditEvent::AuthFailed)
            .count(),
        1
    );

    let text = format!(
        "{}{}{:?}",
        serde_json::to_string(&report).unwrap(),
        serde_json::to_string(&audits.iter().map(|(_, e)| e).collect::<Vec<_>>()).unwrap(),
        report
    );
    assert!(!text.contains(PASSWORD));
    assert!(!text.contains(OWN_ADDRESS));

    h.mailbox.fail_with(None);
    state.clear_halt();
    let resumed = poll(&h, &mut state).await;
    assert_eq!(resumed.accepted, 1);
}

#[tokio::test]
async fn reports_and_audit_hold_no_address_subject_body_or_message_id() {
    let h = Harness::new().await;
    h.mailbox.push(message(1, "<unique-id-123@x>"));
    h.mailbox.push(message(2, "<unique-id-456@x>"));
    h.sink.reject_on(2);
    let mut state = PollState::new();
    let report = poll(&h, &mut state).await;
    let text = format!(
        "{}{}",
        serde_json::to_string(&report).unwrap(),
        serde_json::to_string(&h.audit.entries().iter().map(|(_, e)| e).collect::<Vec<_>>())
            .unwrap(),
    );
    for forbidden in [
        "customer@elsewhere",
        "Secret subject",
        "Secret body",
        "unique-id",
        "Customer Name",
    ] {
        assert!(!text.contains(forbidden), "report leaks {forbidden}");
    }
}

#[tokio::test]
async fn transient_failures_back_off_exponentially_up_to_fifteen_minutes() {
    let h = Harness::new().await;
    h.mailbox.fail_with(Some(MailError::Transient(
        TransientFailure::ConnectionRefused,
    )));
    let mut state = PollState::new();
    let mut delays = Vec::new();
    for _ in 0..8 {
        let report = poll(&h, &mut state).await;
        assert_eq!(report.outcome, PollOutcome::Failed);
        delays.push(report.next_delay.unwrap());
    }
    assert_eq!(delays[0], Duration::from_secs(30));
    assert_eq!(delays[1], Duration::from_secs(60));
    assert_eq!(delays[2], Duration::from_secs(120));
    assert!(delays.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(*delays.last().unwrap(), MAX_BACKOFF);
    assert_eq!(backoff(100), MAX_BACKOFF);
}

#[tokio::test]
async fn a_disabled_account_is_not_polled_and_an_invalid_one_halts() {
    let h = Harness::new().await;
    let mut state = PollState::new();
    let mut off = account();
    off.enabled = false;
    let deps = h.deps("node-a");
    assert_eq!(
        poll_once(&off, h.now(), &mut state, &deps).await.outcome,
        PollOutcome::Disabled
    );
    let mut bad = account();
    bad.address = "x".into();
    let report = poll_once(&bad, h.now(), &mut state, &deps).await;
    assert_eq!(report.outcome, PollOutcome::ConfigRejected);
    assert_eq!(h.mailbox.fetches(), 0);
}

#[tokio::test]
async fn a_missing_credential_halts_the_account() {
    let h = Harness::new().await;
    let mut acct = account();
    acct.credential_ref = "mail/other".into();
    let mut state = PollState::new();
    let deps = h.deps("node-a");
    let report = poll_once(&acct, h.now(), &mut state, &deps).await;
    assert_eq!(report.outcome, PollOutcome::AuthFailed);
    assert_eq!(report.reason, Some("credential_missing"));
    assert_eq!(h.mailbox.fetches(), 0);
}
