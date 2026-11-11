//! Tests of the identity audit trail.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use super::*;
use spot_core::ManualClock;
use spot_sessions::throttle::{FailureRecord, ThrottleStore};
use spot_sessions::{
    ClientInfo, InMemorySessionStore, SaltKey, SessionManager, SessionPolicy, SessionState, UserKey,
};
use spot_storage_contract::RecordId;
use std::time::Duration;

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    ))
}

fn recorder(sink: &Arc<CapturingAudit>, clock: &Arc<ManualClock>) -> Arc<Recorder> {
    Arc::new(Recorder::new("site", sink.clone(), clock.clone()))
}

fn event(topic: &'static str, data: serde_json::Value) -> IdentityEvent {
    IdentityEvent {
        topic,
        subject: None,
        data,
    }
}

fn attempt(account: &str, code: &str, clock: &ManualClock) -> LoginAttempt {
    LoginAttempt {
        site: spot_core::SiteId::new("site").unwrap(),
        account: account.to_owned(),
        source_digest: "ab12cd".into(),
        result_code: code.to_owned(),
        at: clock.now_utc(),
    }
}

#[test]
fn the_attempted_name_is_redacted_and_truncated() {
    assert_eq!(redact_name("  Alice@Example.com "), "Alice@Example.com");
    assert_eq!(redact_name("a b\nc\u{0}d"), "a?b?c?d");
    assert_eq!(redact_name(&"x".repeat(300)), "[redacted]");
    assert_eq!(redact_name(&format!("{}.", "x".repeat(10))).len(), 11);
    let spaced = "a.b ".repeat(100);
    assert_eq!(redact_name(&spaced).chars().count(), ATTEMPT_NAME_MAX);
    assert_eq!(redact_name("spot_abcdefghijkl_secret"), "[redacted]");
    assert!(!redact_name("pass word\"}{").contains('"'));
    for token in [
        "0123456789abcdef0123456789abcdef",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r",
        "aGVsbG8gd29ybGQgdGhpcyBpcyBhIHRva2Vu+abc/def==",
        "SPOT_abc",
    ] {
        assert_eq!(redact_name(token), "[redacted]", "{token}");
    }
}

#[tokio::test]
async fn a_failed_sign_in_is_audited_even_when_the_sink_fails_once() {
    let (sink, clock) = (Arc::new(CapturingAudit::new()), clock());
    let events = AuditingEvents::new(
        recorder(&sink, &clock),
        Arc::new(InMemorySessionStore::new()),
    );
    sink.fail_next(1);
    events
        .emit(event(
            "identity_sign_in_failed",
            serde_json::json!({"reason": "bad_credentials", "password": "hunter2"}),
        ))
        .await
        .unwrap();
    let failed = sink.of("sign_in_failed");
    assert_eq!(failed.len(), 1, "the retry wrote it");
    assert_eq!(failed[0].reason.as_deref(), Some("bad_credentials"));
    assert!(!serde_json::to_string(&failed).unwrap().contains("hunter2"));
    // Both attempts fail: still no error to the caller, and no record.
    sink.fail_all(true);
    events
        .emit(event("identity_sign_in_failed", serde_json::json!({})))
        .await
        .unwrap();
    events
        .record_login_attempt(attempt("bob", "login.invalid_credentials", &clock))
        .await
        .unwrap();
    sink.fail_all(false);
    assert_eq!(sink.records().len(), 1);
}

#[tokio::test]
async fn a_success_whose_audit_cannot_be_written_is_refused_and_its_session_revoked() {
    let (sink, clock) = (Arc::new(CapturingAudit::new()), clock());
    let store = Arc::new(InMemorySessionStore::new());
    let manager = SessionManager::new(
        store.clone() as Arc<dyn SessionStore>,
        SessionPolicy::DEFAULT,
        Arc::new(SaltKey::generate()),
    );
    let user = RecordId::new_v7();
    let key = UserKey::new(user.to_string()).unwrap();
    let started = manager
        .start(
            key.clone(),
            "127.0.0.1",
            ClientInfo::default(),
            1_800_000_000,
        )
        .await
        .unwrap();
    let id = started.session.id.clone();
    let events = AuditingEvents::new(recorder(&sink, &clock), store.clone());
    let success = || IdentityEvent {
        topic: "identity_sign_in_succeeded",
        subject: Some(user),
        data: serde_json::json!({"session": id, "method": "password"}),
    };

    sink.fail_all(true);
    let refused = events.emit(success()).await;
    assert!(refused.is_err(), "fail closed");
    let after = store.list_for_user(&key).await.unwrap();
    assert!(
        matches!(after[0].state, SessionState::Revoked { .. }),
        "the unauditable session is revoked"
    );
    assert!(sink.records().is_empty());

    // One failed write is retried and the sign-in stands.
    sink.fail_all(false);
    sink.fail_next(1);
    events.emit(success()).await.unwrap();
    assert_eq!(sink.of("sign_in_succeeded").len(), 1);
}

#[tokio::test]
async fn attempt_records_keep_a_redacted_name_and_never_the_credential() {
    let (sink, clock) = (Arc::new(CapturingAudit::new()), clock());
    let events = AuditingEvents::new(
        recorder(&sink, &clock),
        Arc::new(InMemorySessionStore::new()),
    );
    let long = format!("{}\nINJECTED", "a b ".repeat(80));
    events
        .record_login_attempt(attempt(&long, "login.account_locked", &clock))
        .await
        .unwrap();
    let refused = sink.of("sign_in_refused");
    assert_eq!(refused.len(), 1);
    let name = refused[0].attempted.clone().unwrap();
    assert_eq!(name.chars().count(), ATTEMPT_NAME_MAX);
    assert!(!name.contains('\n'));
    assert_eq!(refused[0].reason.as_deref(), Some("account_locked"));
}

#[test]
fn failures_over_the_budget_are_counted_and_summarised() {
    let (sink, clock) = (Arc::new(CapturingAudit::new()), clock());
    let rec = Recorder::with_budgets("site", sink.clone(), clock.clone(), 3, 1);
    for _ in 0..10 {
        rec.failure(&rec.record("sign_in_failed"));
    }
    assert_eq!(sink.of("sign_in_failed").len(), 3, "bounded per minute");
    // An authenticated record is never dropped.
    rec.always(&rec.record("password_changed"));
    assert_eq!(sink.of("password_changed").len(), 1);
    clock.advance(Duration::from_secs(61));
    rec.failure(&rec.record("sign_in_failed"));
    let summary = sink.of("failures_suppressed");
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].count, Some(7));
    assert_eq!(sink.of("sign_in_failed").len(), 4);
    // Lockouts have their own budget.
    rec.lockout(&rec.record("lockout_started"));
    rec.lockout(&rec.record("lockout_started"));
    assert_eq!(sink.of("lockout_started").len(), 1);
}

#[tokio::test]
async fn lockout_start_and_end_are_audited_and_the_lock_holds_when_the_audit_fails() {
    let (sink, clock) = (Arc::new(CapturingAudit::new()), clock());
    let store = AuditedThrottleStore::new(
        Arc::new(BoundedThrottleStore::new(clock.clone())),
        recorder(&sink, &clock),
    );
    let now = clock.now_utc().unix_timestamp();
    let lock = FailureRecord {
        failures: 5,
        window_start: now,
        locked_until: now + 60,
        last_failure: now,
    };
    sink.fail_all(true);
    store.put(true, b"alice", lock).await.unwrap();
    assert_eq!(
        store.get(true, b"alice").await.unwrap().locked_until,
        now + 60
    );
    sink.fail_all(false);
    let again = FailureRecord {
        locked_until: now + 120,
        ..lock
    };
    store.put(true, b"alice", again).await.unwrap();
    let started = sink.of("lockout_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].attempted.as_deref(), Some("alice"));
    assert_eq!(started[0].count, Some(5));
    // A source counter is not a lockout.
    store.put(false, b"digest", again).await.unwrap();
    assert_eq!(sink.of("lockout_started").len(), 1);
    clock.advance(Duration::from_secs(200));
    store.clear(true, b"alice").await.unwrap();
    let ended = sink.of("lockout_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].reason.as_deref(), Some("expired"));
}

#[tokio::test]
async fn the_counters_have_a_ceiling_and_never_evict_a_live_lockout() {
    let clock = clock();
    let now = clock.now_utc().unix_timestamp();
    let store = BoundedThrottleStore::with_cap(clock.clone(), 3);
    let failed = FailureRecord {
        failures: 1,
        window_start: now,
        locked_until: 0,
        last_failure: now,
    };
    let locked = FailureRecord {
        locked_until: now + 600,
        ..failed
    };
    store.put(true, b"locked", locked).await.unwrap();
    for name in [&b"a"[..], b"b", b"c", b"d", b"e"] {
        store.put(true, name, failed).await.unwrap();
    }
    assert_eq!(store.len(), 3, "bounded");
    assert_eq!(store.get(true, b"locked").await.unwrap(), locked);
    assert_eq!(
        store.get(true, b"a").await.unwrap(),
        FailureRecord::default()
    );
    assert_eq!(store.get(true, b"e").await.unwrap(), failed);
    // Only live lockouts left: a new key is refused (fail closed), nothing is evicted.
    let full = BoundedThrottleStore::with_cap(clock, 2);
    full.put(true, b"x", locked).await.unwrap();
    full.put(true, b"y", locked).await.unwrap();
    assert!(full.put(true, b"z", failed).await.is_err());
    assert_eq!(full.get(true, b"x").await.unwrap(), locked);
    // Updating a key that is already tracked still works.
    full.put(true, b"x", locked).await.unwrap();
}

#[test]
fn the_file_sink_appends_json_lines_and_stays_inside_its_size_bound() {
    let dir = std::env::temp_dir().join(format!("spot-audit-{}", RecordId::new_v7()));
    let path = dir.join("identity-audit.log");
    let sink = FileAudit::open_with(&path, 4096).unwrap();
    let rec = Recorder::new("site", Arc::new(sink), clock());
    for _ in 0..400 {
        let mut record = rec.record("sign_in_failed");
        record.attempted = Some("alice".into());
        rec.always(&record);
    }
    let mut total = 0;
    for n in 0..=FILE_KEEP + 1 {
        let p = if n == 0 {
            path.clone()
        } else {
            PathBuf::from(format!("{}.{n}", path.display()))
        };
        let Ok(text) = std::fs::read_to_string(&p) else {
            assert!(n > FILE_KEEP, "file {n} should exist");
            continue;
        };
        assert!(text.len() <= 4096);
        for line in text.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["kind"], "sign_in_failed");
        }
        total += text.len();
    }
    assert!(total <= 4096 * (FILE_KEEP + 1));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn the_audit_file_is_never_a_symlink_and_an_existing_file_is_tightened() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("spot-audit-link-{}", RecordId::new_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("elsewhere");
    std::fs::write(&target, "").unwrap();
    let link = dir.join("identity-audit.log");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(FileAudit::open(&link).is_err());
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "").unwrap();
    std::fs::set_permissions(&link, std::fs::Permissions::from_mode(0o644)).unwrap();
    FileAudit::open(&link).unwrap();
    let mode = std::fs::metadata(&link).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- refused API keys (WP-PRD-01.T15) ----------------------------------------------------------

fn key_record(
    stored: &spot_auth::StoredApiKey,
    state: spot_login::KeyState,
    expires_at: Option<time::OffsetDateTime>,
) -> spot_login::KeyRecord {
    spot_login::KeyRecord {
        key_id: stored.key_id.clone(),
        digest: spot_auth::TokenDigest::from_bytes(stored.digest.as_bytes()).unwrap(),
        owner: RecordId::new_v7(),
        state,
        expires_at,
    }
}

#[test]
fn a_refused_key_is_classified_without_naming_which_ids_exist() {
    let now = clock().now_utc();
    let (key, stored) = spot_auth::ApiKey::generate();
    let (other, _) = spot_auth::ApiKey::generate();
    let text = key.text();
    let active = key_record(&stored, spot_login::KeyState::Active, None);
    // Right secret: revoked and expired are named.
    let revoked = key_record(&stored, spot_login::KeyState::Revoked, None);
    let expired = key_record(
        &stored,
        spot_login::KeyState::Active,
        Some(now - time::Duration::seconds(1)),
    );
    assert_eq!(
        classify(&text, Some(&revoked), now),
        RejectionReason::Revoked
    );
    assert_eq!(
        classify(&text, Some(&expired), now),
        RejectionReason::Expired
    );
    // A wrong secret for a known id, whatever the key's state, is `unknown`.
    let wrong = format!("spk_{}_{}", key.key_id(), "B".repeat(43));
    for record in [&active, &revoked, &expired] {
        assert_eq!(
            classify(&wrong, Some(record), now),
            RejectionReason::Unknown
        );
    }
    assert_eq!(classify(&other.text(), None, now), RejectionReason::Unknown);
    assert_eq!(classify("spk_nope", None, now), RejectionReason::Malformed);
    assert_eq!(
        classify("spk_nope", Some(&active), now),
        RejectionReason::Malformed
    );
}

#[test]
fn a_rejection_record_has_the_public_id_only_and_the_serialised_bytes_hold_no_secret() {
    let (sink, clock) = (Arc::new(CapturingAudit::new()), clock());
    let rec = recorder(&sink, &clock);
    let (key, _) = spot_auth::ApiKey::generate();
    let text = key.text();
    // The 43-character secret part, which can itself contain `_`: split into three parts, not
    // on the last `_` (that can leave a one-character fragment, found by chance in any text).
    let secret = text.splitn(3, '_').nth(2).unwrap().to_owned();
    assert_eq!(secret.len(), spot_auth::api_key::API_KEY_SECRET_LENGTH);
    rejected(&rec, &text, RejectionReason::Unknown, PathClass::Metrics);
    rejected(
        &rec,
        &format!("spk_{secret}"),
        RejectionReason::Malformed,
        PathClass::Api,
    );
    rejected(
        &rec,
        "spk_\u{1F600}\n_x",
        RejectionReason::Malformed,
        PathClass::Realtime,
    );
    let records = sink.of("api_key_rejected");
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].key_id.as_deref(), Some(key.key_id()));
    assert!(records[0].key_id.as_ref().unwrap().chars().count() <= KEY_ID_MAX);
    assert_eq!(records[0].path.as_deref(), Some("metrics"));
    assert_eq!(
        records[1].key_id, None,
        "an unparseable credential records no id"
    );
    assert_eq!(records[2].key_id, None);
    let bytes = serde_json::to_string(&records).unwrap();
    assert!(
        !bytes.contains(&secret) && !bytes.contains(&text),
        "{bytes}"
    );
    assert!(!format!("{records:?}").contains(&secret));
}

#[test]
fn rejections_share_the_failure_budget_and_a_dead_sink_does_not_panic() {
    let (sink, clock) = (Arc::new(CapturingAudit::new()), clock());
    let rec = Recorder::with_budgets("site", sink.clone(), clock.clone(), 4, 1);
    // Sign-in failures and key rejections draw on the same per-minute budget.
    rec.failure(&rec.record("sign_in_failed"));
    rec.failure(&rec.record("sign_in_failed"));
    for _ in 0..10 {
        rejected(&rec, "spk_x", RejectionReason::Malformed, PathClass::Api);
    }
    assert_eq!(sink.of("api_key_rejected").len(), 2);
    clock.advance(Duration::from_secs(61));
    rejected(&rec, "spk_x", RejectionReason::Malformed, PathClass::Api);
    sink.fail_all(true);
    rejected(&rec, "spk_x", RejectionReason::Malformed, PathClass::Api);
    sink.fail_all(false);
    let summary = sink.of("failures_suppressed");
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].count, Some(8));
    assert_eq!(PathClass::of_path("/metrics"), PathClass::Metrics);
    assert_eq!(PathClass::of_path("/api/v1/realtime"), PathClass::Realtime);
    assert_eq!(PathClass::of_path("/api/v1/x?y=1"), PathClass::Api);
}
