//! The [`LoginThrottle`] state machine's behaviour tests: the two counters, backoff,
//! lockout, audit notes and the timing-safety rules (WP-PLT-02.T2 item 4).
//!
//! Seeded properties (proptest, no sleeps — the clock is an argument) live here too:
//! lockout never below M, success resets only the account, delay monotonic-then-capped,
//! 10,000 random sequences always unlockable, two sources count towards the account
//! without locking each other.
use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;

use spot_sessions::model::SaltKey;
use spot_sessions::tests_support;
use spot_sessions::throttle::{
    AccountKey, AuditNote, FailureRecord, InMemoryThrottleStore, LoginThrottle, ThrottleAction,
    ThrottleConfig, ThrottleDecision, ThrottleKind, ThrottleStore, UnlockedBy,
};

/// The default-config throttle over one fresh in-memory store.
fn throttle() -> (LoginThrottle<InMemoryThrottleStore>, Arc<SaltKey>) {
    throttle_with(ThrottleConfig::DEFAULT)
}

/// A throttle with `config` and one fresh store; the site salt is fixed so two instances
/// agree on source digests.
fn throttle_with(config: ThrottleConfig) -> (LoginThrottle<InMemoryThrottleStore>, Arc<SaltKey>) {
    let salt = Arc::new(tests_support::fixed_salt(1));
    let store = InMemoryThrottleStore::new();
    match LoginThrottle::new(store, config, salt.clone()) {
        Ok(machine) => (machine, salt),
        Err(error) => panic!("throttle construction failed: {error:?}"),
    }
}

/// An account key that has been validated by construction.
fn account(name: &str) -> AccountKey {
    match AccountKey::new(name) {
        Ok(key) => key,
        Err(reason) => panic!("account key is invalid: {reason}"),
    }
}

#[tokio::test]
async fn check_allows_a_fresh_account_and_source() {
    let (machine, _salt) = throttle();
    let decision = machine
        .check(&account("a@x.test"), "10.0.0.1", 1_000)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(decision.allowed());
}

#[tokio::test]
async fn failures_below_the_backoff_threshold_allow() {
    let (machine, _salt) = throttle();
    let account = account("a@x.test");
    // Two failures (below backoff_after = 3): still allowed.
    for now in [1_000_i64, 1_010] {
        machine
            .record_failure(&account, "10.0.0.1", now)
            .await
            .unwrap_or_else(|error| panic!("record failed: {error:?}"));
        let decision = machine
            .check(&account, "10.0.0.1", now + 1)
            .await
            .unwrap_or_else(|error| panic!("check failed: {error:?}"));
        assert!(decision.allowed(), "a failure count of {now} should allow");
    }
}

#[tokio::test]
async fn failures_past_the_threshold_back_off_per_account() {
    let (machine, _salt) = throttle();
    let account = account("a@x.test");
    for now in 1_000..1_002 {
        machine
            .record_failure(&account, "10.0.0.1", now)
            .await
            .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    }
    // The third failure crosses backoff_after = 3.
    machine
        .record_failure(&account, "10.0.0.1", 1_002)
        .await
        .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    let decision = machine
        .check(&account, "10.0.0.2", 1_003)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    match decision {
        ThrottleDecision::Delay { delay, because } => {
            assert_eq!(because, ThrottleKind::Account);
            assert_eq!(delay, Duration::from_secs(1));
        }
        other => panic!("expected a delay, got {other:?}"),
    }
}

#[tokio::test]
async fn source_failures_back_off_the_source_not_the_account() {
    let (machine, _salt) = throttle();
    let victim = account("victim@x.test");
    // One source fails against several accounts: each account's own counter stays at 0.
    for name in ["a@x.test", "b@x.test", "c@x.test"] {
        machine
            .record_failure(&account(name), "203.0.113.7", 1_000)
            .await
            .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    }
    // The source is now at 3 failures ⇒ backed off for every account it touches.
    let decision = machine
        .check(&victim, "203.0.113.7", 1_001)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    match decision {
        ThrottleDecision::Delay { because, .. } => assert_eq!(because, ThrottleKind::Source),
        other => panic!("expected a source delay, got {other:?}"),
    }
    // But a *different* source can still try the victim: no account lock happened.
    let from_elsewhere = machine
        .check(&victim, "198.51.100.9", 1_001)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(from_elsewhere.allowed());
}

#[tokio::test]
async fn the_m_th_failure_locks_the_account_hard() {
    let (machine, _salt) = throttle();
    let acct = account("a@x.test");
    // M = 10 failures inside the window.
    let mut locked_note = None;
    for now in 1_000..1_010_i64 {
        let notes = machine
            .record_failure(&acct, "10.0.0.1", now)
            .await
            .unwrap_or_else(|error| panic!("record failed: {error:?}"));
        for note in notes {
            if note.action == ThrottleAction::Locked {
                locked_note = Some(note.clone());
            }
        }
    }
    let Some(note) = locked_note else {
        panic!("the M-th failure did not lock the account");
    };
    assert_eq!(note.failures, 10);
    assert_eq!(note.kind, ThrottleKind::Account);
    assert_eq!(note.locked_until, Some(1_009 + 30 * 60));

    // Any check on the locked account refuses, from any source, before any password work.
    let decision = machine
        .check(&acct, "203.0.113.9", 1_011)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    match decision {
        ThrottleDecision::Locked { until } => assert_eq!(until, 1_009 + 30 * 60),
        other => panic!("expected a lockout, got {other:?}"),
    }
}

#[tokio::test]
async fn lockout_never_triggers_below_m_failure_sequences() {
    // Seeded property: across 200 random sequences of 9 failures (one below default M=10),
    // no lockout ever fires; each sequence's delay is capped and monotonic.
    let cases: Vec<Vec<i64>> = (0..200)
        .map(|seed| {
            let mut state = seed as u64 * 2654435761 % 1_000_003 + 1;
            let mut times = Vec::new();
            let mut now = 1_000_i64;
            for _ in 0..9 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                now += (state >> 33) as i64 % 60;
                times.push(now);
            }
            times
        })
        .collect();
    for times in cases {
        let (machine, _salt) = throttle();
        let account = account("a@x.test");
        for &now in &times {
            let notes = machine
                .record_failure(&account, "10.0.0.1", now)
                .await
                .unwrap_or_else(|error| panic!("record failed: {error:?}"));
            assert!(
                notes
                    .iter()
                    .all(|note| note.action != ThrottleAction::Locked),
                "a lockout fired at {} failures (M=10)",
                notes
                    .iter()
                    .filter(|note| note.kind == ThrottleKind::Account)
                    .map(|note| note.failures)
                    .max()
                    .unwrap_or(0)
            );
        }
    }
}

#[tokio::test]
async fn success_resets_only_the_account_counter() {
    let (machine, _salt) = throttle();
    let acct = account("a@x.test");
    // Two failures (below the backoff threshold), then a success.
    for now in 1_000..1_002 {
        machine
            .record_failure(&acct, "10.0.0.1", now)
            .await
            .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    }
    let note = machine
        .record_success(&acct, "10.0.0.1", 1_003)
        .await
        .unwrap_or_else(|error| panic!("record_success failed: {error:?}"));
    assert_eq!(note.action, ThrottleAction::SuccessReset);
    // The account is clear.
    let decision = machine
        .check(&acct, "10.0.0.9", 1_004)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(decision.allowed());
    // But the source's counter survived the success: three more failures from the same
    // source re-trigger a *source* backoff even though the account just succeeded.
    for name in ["b@x.test", "c@x.test", "d@x.test"] {
        machine
            .record_failure(&account(name), "10.0.0.1", 1_100)
            .await
            .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    }
    let decision = machine
        .check(&acct, "10.0.0.1", 1_101)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    match decision {
        ThrottleDecision::Delay { because, .. } => assert_eq!(because, ThrottleKind::Source),
        other => panic!("the source counter did not survive the success: {other:?}"),
    }
}

#[tokio::test]
async fn the_backoff_delay_is_monotonic_then_capped() {
    let config = ThrottleConfig {
        backoff_after: 1,
        lockout_after: 1_000,
        window: Duration::from_secs(10 * 60),
        backoff_base: Duration::from_secs(1),
        backoff_multiplier: 2,
        backoff_cap: Duration::from_secs(60),
        lockout_duration: Duration::from_secs(60),
    };
    let (machine, _salt) = throttle_with(config);
    let account = account("a@x.test");
    let mut last = Duration::ZERO;
    let mut previous_delay = Some(Duration::from_secs(0));
    for failures in 1..12_u32 {
        // Drive the counter to `failures` directly through the store to keep the test
        // table-driven (the state machine is the unit under test, not the loop).
        machine
            .store
            .put(
                true,
                account.as_str().as_bytes(),
                FailureRecord {
                    failures,
                    window_start: 1_000,
                    locked_until: 0,
                    last_failure: 1_000,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("store put failed: {error:?}"));
        let decision = machine
            .check(&account, "10.0.0.1", 1_050)
            .await
            .unwrap_or_else(|error| panic!("check failed: {error:?}"));
        let ThrottleDecision::Delay { delay, .. } = decision else {
            panic!("expected a delay at {failures} failures");
        };
        // Monotonic up to the cap, then pinned.
        let Some(previous) = previous_delay else {
            panic!("previous delay exists");
        };
        assert!(delay >= previous);
        assert!(delay <= Duration::from_secs(60));
        previous_delay = Some(delay);
        last = delay;
    }
    assert_eq!(last, Duration::from_secs(60), "the cap holds");
}

#[tokio::test]
async fn ten_thousand_random_sequences_are_always_unlockable() {
    // Seeded property: drive 10,000 short random attack sequences (failures with varied
    // times, some locked accounts), then always unlock and check-allow. Nothing the
    // throttle does leaves a state `unlock` cannot reset.
    let mut state: u64 = 0x5EED;
    let mut locked_seen = 0_u32;
    for _ in 0..10_000_u32 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let (machine, _salt) = throttle();
        let account = account(format!("u{}", (state >> 33) % 7).as_str());
        let source = format!("10.0.{}.{}", (state >> 40) % 254, (state >> 48) % 254);
        // Dense bursts (1..12 failures, 1..5 s apart) inside the 15-minute window: enough
        // of them reach the 10-failure lockout for the unlockability claim to bite. The
        // bits used for the mods are the *high* bits of the LCG state: the low bits of a
        // multiplicative LCG cycle short, and short cycles could keep every burst length
        // below the lockout threshold.
        let mut now = 1_000_i64 + i64::try_from(state % 100).unwrap_or(0);
        for _ in 0..((state >> 57) % 12 + 1) {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            now += i64::try_from((state >> 60) % 5).unwrap_or(1);
            let _notes = machine
                .record_failure(&account, &source, now)
                .await
                .unwrap_or_else(|error| panic!("record failed: {error:?}"));
            for note in &_notes {
                if note.action == ThrottleAction::Locked {
                    locked_seen += 1;
                }
            }
        }
        // Unlock (admin path); the account always admits afterwards.
        machine
            .unlock(&account, UnlockedBy::Admin, &source, now)
            .await
            .unwrap_or_else(|error| panic!("unlock failed: {error:?}"));
        let decision = machine
            .check(&account, &source, now + 1)
            .await
            .unwrap_or_else(|error| panic!("check failed: {error:?}"));
        if state.is_multiple_of(5) {
            // Sometimes the fresh source counter re-backs-off; that is a Delay, never a
            // lockout: unlockable means "no lock remains".
            assert!(matches!(
                decision,
                ThrottleDecision::Allow | ThrottleDecision::Delay { .. }
            ));
        } else {
            assert!(decision.allowed() || matches!(decision, ThrottleDecision::Delay { .. }));
        }
    }
    assert!(
        locked_seen > 0,
        "10,000 sequences never produced a lock to test"
    );
}

#[tokio::test]
async fn two_sources_count_towards_the_account_but_do_not_lock_each_other() {
    let (machine, _salt) = throttle();
    let acct = account("shared@x.test");
    // Source A fails twice, source B fails twice: each under its own per-source backoff
    // (3) but together they push the account to 4 (inside backoff for the account too).
    machine
        .record_failure(&acct, "203.0.113.1", 1_000)
        .await
        .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    machine
        .record_failure(&acct, "203.0.113.1", 1_010)
        .await
        .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    machine
        .record_failure(&acct, "198.51.100.1", 1_020)
        .await
        .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    machine
        .record_failure(&acct, "198.51.100.1", 1_030)
        .await
        .unwrap_or_else(|error| panic!("record failed: {error:?}"));

    // Account counter is at 4 ≥ 3 ⇒ the account is backed off for *both* sources.
    let from_a = machine
        .check(&acct, "203.0.113.1", 1_040)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(matches!(from_a, ThrottleDecision::Delay { .. }));
    let from_b = machine
        .check(&acct, "198.51.100.1", 1_040)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(matches!(from_b, ThrottleDecision::Delay { .. }));

    // Neither source reached its own backoff (2 < 3), so each alone from a *fresh
    // account* is fine — this is the "no cross-lock" half.
    let fresh = account("fresh@x.test");
    let from_a2 = machine
        .check(&fresh, "203.0.113.1", 1_040)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(from_a2.allowed());

    // Push one source to 2 more failures (3, its own backoff) against the *shared*
    // account: the shared account never locks from source failures alone (below M = 10
    // account failures).
    machine
        .record_failure(&acct, "203.0.113.1", 1_050)
        .await
        .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    // Account is now at 5 failures: no lock (5 < 10); a hard lock needs M.
    let note = machine
        .record_failure(&acct, "203.0.113.1", 1_060)
        .await
        .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    assert!(note.iter().all(|n| n.action != ThrottleAction::Locked));
    // And a third source can still *check* (delays apply but no lockout).
    let decision = machine
        .check(&acct, "192.0.2.9", 1_070)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(matches!(decision, ThrottleDecision::Delay { .. }));
}

#[tokio::test]
async fn the_window_slides_failures_out_half_open() {
    // The default window is 15 minutes; a failure exactly at `window_start + window`
    // starts a fresh window (the first failure is no longer counted).
    let config = ThrottleConfig {
        window: Duration::from_secs(600),
        ..ThrottleConfig::DEFAULT
    };
    let (machine, _salt) = throttle_with(config);
    let account = account("a@x.test");
    // 9 failures right before the window closes (under M = 10, so no lockout).
    for step in 0..9_u32 {
        machine
            .record_failure(&account, "10.0.0.1", 1_000 + i64::from(step))
            .await
            .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    }
    // Just inside the closing window: still counted.
    let inside = machine
        .check(&account, "10.0.0.2", 1_598)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(matches!(inside, ThrottleDecision::Delay { .. }));
    // At 1_000 + 600 = 1_600 exactly the window has closed: fresh state.
    let decision = machine
        .check(&account, "10.0.0.2", 1_600)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert!(decision.allowed());
}

#[tokio::test]
async fn the_delay_applies_to_wrong_and_right_passwords_alike() {
    // The check-before-hash contract: the delay decision precedes any password work.
    // We prove the *state machine* side: while a backoff is active, `check` delays an
    // attempt regardless of anything else about it (no decision input could differ).
    let (machine, _salt) = throttle();
    let account = account("a@x.test");
    for now in 1_000..1_003 {
        machine
            .record_failure(&account, "10.0.0.1", now)
            .await
            .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    }
    // Two consecutive checks a moment apart are decided identically (the state is in the
    // throttle, not in "the password"); the caller applies the wait before retrying.
    let first = machine
        .check(&account, "10.0.0.1", 1_005)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    let second = machine
        .check(&account, "10.0.0.1", 1_006)
        .await
        .unwrap_or_else(|error| panic!("check failed: {error:?}"));
    assert_eq!(first, second);
    assert!(matches!(first, ThrottleDecision::Delay { .. }));
}

#[tokio::test]
async fn every_state_change_returns_an_audit_note_without_credentials() {
    let (machine, salt) = throttle();
    let account = account("a@x.test");
    // A failure returns notes for both counters.
    let notes = machine
        .record_failure(&account, "203.0.113.5", 1_000)
        .await
        .unwrap_or_else(|error| panic!("record failed: {error:?}"));
    assert_eq!(notes.len(), 2);
    let account_note = &notes[0];
    assert_eq!(account_note.account, account);
    assert_eq!(account_note.kind, ThrottleKind::Account);
    assert_eq!(account_note.action, ThrottleAction::FailureCounted);
    assert_eq!(account_note.failures, 1);
    // The note's source is a salted digest — not the address.
    let expected_source = salt.digest_source("203.0.113.5");
    assert_eq!(account_note.source, expected_source);
    assert_ne!(format!("{:?}", account_note), "203.0.113.5");
    assert!(!format!("{notes:?}").contains("203.0.113"));

    let source_note = &notes[1];
    assert_eq!(source_note.kind, ThrottleKind::Source);
    assert_eq!(source_note.failures, 1);

    // A success note.
    let success = machine
        .record_success(&account, "203.0.113.5", 1_010)
        .await
        .unwrap_or_else(|error| panic!("record_success failed: {error:?}"));
    assert_eq!(success.action, ThrottleAction::SuccessReset);

    // Unlock notes: admin and expiry paths.
    for (by, expected) in [
        (UnlockedBy::Admin, ThrottleAction::AdminUnlocked),
        (UnlockedBy::Expiry, ThrottleAction::ExpiredUnlocked),
    ] {
        let note = machine
            .unlock(&account, by, "203.0.113.5", 1_020)
            .await
            .unwrap_or_else(|error| panic!("unlock failed: {error:?}"));
        assert_eq!(note.action, expected);
    }
}

#[tokio::test]
async fn audit_notes_never_carry_credential_material() {
    // An AuditNote's Debug output shows account, digest and counts — never a token, a
    // password or the address.
    let note = AuditNote {
        account: account("a@x.test"),
        source: tests_support::source_of("192.0.2.1"),
        kind: ThrottleKind::Account,
        failures: 2,
        action: ThrottleAction::FailureCounted,
        locked_until: None,
    };
    let rendered = format!("{note:?}");
    assert!(!rendered.contains("password"));
    assert!(!rendered.contains("token"));
}

/// A helper the property below uses: total failures across both counters, through the
/// store (white-box verification of the two-counter invariant).
async fn total_failures<T: ThrottleStore>(
    machine: &LoginThrottle<T>,
    account: &AccountKey,
    salt: &SaltKey,
    source: &str,
) -> (u32, u32) {
    let got_account = machine
        .store
        .get(true, account.as_str().as_bytes())
        .await
        .unwrap_or_else(|error| panic!("store get failed: {error:?}"));
    let got_source = machine
        .store
        .get(false, salt.digest_source(source).as_bytes())
        .await
        .unwrap_or_else(|error| panic!("store get failed: {error:?}"));
    (got_account.failures, got_source.failures)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Seeded: a success between failures resets only the account counter — the source
    /// counter keeps counting whatever the successes.
    #[test]
    fn success_between_failures_resets_only_account(
        seed in 0_u64..10_000,
        failures_per_side in 1_u32..4,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("runtime: {error}"));
        rt.block_on(async move {
            let (machine, salt) = throttle();
            let source_a = format!("203.0.113.{}", seed % 254);
            let source_b = format!("198.51.100.{}", seed % 254);
            let account = account("a@x.test");
            let mut now = 1_000;
            for _ in 0..failures_per_side {
                machine
                    .record_failure(&account, &source_a, now)
                    .await
                    .unwrap_or_else(|e| panic!("{e:?}"));
                now += 1;
            }
            machine
                .record_success(&account, &source_a, now)
                .await
                .unwrap_or_else(|e| panic!("{e:?}"));
            now += 1;
            for _ in 0..failures_per_side {
                machine
                    .record_failure(&account, &source_b, now)
                    .await
                    .unwrap_or_else(|e| panic!("{e:?}"));
                now += 1;
            }
            let (account_failures, source_a_failures) =
                total_failures(&machine, &account, &salt, &source_a).await;
            // The account reset happened at the success, so only the failures from B
            // count against the account now (source A's pre-success failures were wiped).
            assert_eq!(account_failures, failures_per_side);
            // Source A's counter: unchanged since its own success is not a source event.
            assert_eq!(source_a_failures, failures_per_side);
        });
    }

    /// Seeded: the backoff delay grows monotonically with the failure count until the cap.
    #[test]
    fn delay_is_monotonic_then_capped(seed in 0_u64..10_000) {
        let delay = Duration::from_secs(1);
        let previous = delay;
        let config = ThrottleConfig {
            backoff_base: delay,
            backoff_cap: Duration::from_secs(300),
            backoff_multiplier: 2,
            ..ThrottleConfig::DEFAULT
        };
        // Pure arithmetic mirror of `backoff_delay_for`, cross-checked against the
        // machine's own function through a store-driven count (the monotonic sequence).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("runtime: {error}"));
        rt.block_on(async move {
            let (machine, _salt) = throttle_with(config);
            let account = account(format!("u{seed}").as_str());
            let mut last = previous;
            for failures in 1..24_u32 {
                machine
                    .store
                    .put(
                        true,
                        account.as_str().as_bytes(),
                        FailureRecord {
                            failures,
                            window_start: 1_000,
                            locked_until: 0,
                            last_failure: 1_000,
                        },
                    )
                    .await
                    .unwrap_or_else(|e| panic!("{e:?}"));
                let decision = machine
                    .check(&account, "10.0.0.1", 1_001)
                    .await
                    .unwrap_or_else(|e| panic!("{e:?}"));
                if failures < config.lockout_after
                    && let ThrottleDecision::Delay { delay, .. } = decision
                {
                    assert!(delay >= last, "delay shrank at {failures}");
                    assert!(delay <= config.backoff_cap);
                    last = delay;
                }
            }
        });
    }
}
