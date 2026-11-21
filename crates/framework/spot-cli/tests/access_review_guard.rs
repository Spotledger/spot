//! The host guard of the access review service (WP-SEC-02.T6): the permission engine call fails
//! closed (engine error, no role snapshot, no answer in time), refusal records are rate-limited
//! per principal and action with a summary when the window closes, and the override slice of the
//! analysis comes only from `active_overrides(now)`. Every operation of the review API has a
//! guard: the table below is `OPERATIONS`, so a new operation is exercised here too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_core::ManualClock;
use spot_http::api::service::BoxFuture;
use spot_sod::SodOverride;
use spot_sod::store::StoredOverride;
use spot_tenancy::TenantId;
use time::macros::datetime;

use spot_cli::services::access_review::OPERATIONS;
use spot_cli::services::access_review_guard::{
    AccessGuard, CODE_NO_ROLE_SNAPSHOT, CODE_NOT_PERMITTED, CODE_PERMISSION_UNAVAILABLE,
    GuardRefusal, RefusalAudit, RefusalLimiter, RefusalRecord, ReviewPermissions, Verdict,
    action_of,
};

#[derive(Clone, Copy)]
enum Mode {
    Allow,
    Deny,
    NoSnapshot,
    Error,
    Hang,
}

struct Engine(Mutex<Mode>);

impl ReviewPermissions for Engine {
    fn decide<'a>(
        &'a self,
        principal: &'a str,
        _action: &'static str,
    ) -> BoxFuture<'a, Result<Verdict, String>> {
        let mode = *self.0.lock().unwrap();
        let granted = principal != "mallory";
        Box::pin(async move {
            match mode {
                Mode::Allow if granted => Ok(Verdict::Allowed),
                Mode::Allow | Mode::Deny => Ok(Verdict::Denied),
                Mode::NoSnapshot => Ok(Verdict::NoSnapshot),
                Mode::Error => Err("engine down".into()),
                Mode::Hang => std::future::pending().await,
            }
        })
    }
}

#[derive(Default)]
struct Audit(Mutex<Vec<RefusalRecord>>);

impl RefusalAudit for Audit {
    fn write<'a>(&'a self, record: RefusalRecord) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.0.lock().unwrap().push(record) })
    }
}

struct Fixture {
    guard: AccessGuard,
    engine: Arc<Engine>,
    audit: Arc<Audit>,
    clock: Arc<ManualClock>,
}

fn fixture(mode: Mode) -> Fixture {
    let engine = Arc::new(Engine(Mutex::new(mode)));
    let audit = Arc::new(Audit::default());
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-12 10:00 UTC)));
    let tenant = TenantId::new_v7(1_800_000_000_000, [3; 10]);
    let guard = AccessGuard::new(engine.clone(), audit.clone(), clock.clone(), &tenant)
        .with_deadline(Duration::from_millis(50));
    Fixture {
        guard,
        engine,
        audit,
        clock,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_operation_is_guarded_and_a_held_permission_passes() {
    let f = fixture(Mode::Allow);
    for op in OPERATIONS {
        assert!(!action_of(op).is_empty());
        f.guard.authorise("alice", op).await.unwrap();
        let refused = f.guard.authorise("mallory", op).await.unwrap_err();
        assert_eq!(refused, GuardRefusal::NotPermitted, "{op:?}");
    }
    assert!(
        f.audit
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|r| matches!(r, RefusalRecord::Refused { .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_engine_error_a_missing_snapshot_and_a_timeout_are_refusals_never_a_pass() {
    for (mode, expected, code) in [
        (
            Mode::Error,
            GuardRefusal::PermissionUnavailable,
            CODE_PERMISSION_UNAVAILABLE,
        ),
        (
            Mode::NoSnapshot,
            GuardRefusal::NoRoleSnapshot,
            CODE_NO_ROLE_SNAPSHOT,
        ),
        (
            Mode::Hang,
            GuardRefusal::PermissionUnavailable,
            CODE_PERMISSION_UNAVAILABLE,
        ),
        (Mode::Deny, GuardRefusal::NotPermitted, CODE_NOT_PERMITTED),
    ] {
        let f = fixture(mode);
        for op in OPERATIONS {
            let refusal = f.guard.authorise("alice", op).await.unwrap_err();
            assert_eq!(refusal, expected);
            assert_eq!(refusal.code(), code);
        }
        let records = f.audit.0.lock().unwrap();
        assert_eq!(records.len(), OPERATIONS.len(), "one record per refusal");
        assert!(
            records
                .iter()
                .all(|r| matches!(r, RefusalRecord::Refused { code: c, .. } if *c == code))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hundred_refusals_in_a_window_write_the_budget_and_one_summary() {
    let f = fixture(Mode::Deny);
    let op = OPERATIONS[0];
    for _ in 0..100 {
        assert!(
            f.guard.authorise("alice", op).await.is_err(),
            "every refusal is still returned"
        );
    }
    assert_eq!(f.audit.0.lock().unwrap().len(), 20);
    assert_eq!(f.guard.limiter().suppressed_total(), 80);
    // The window closes: one summary with the count, written by the tick's flush.
    f.clock.advance(Duration::from_secs(61));
    f.guard.flush().await;
    let records = f.audit.0.lock().unwrap();
    assert_eq!(records.len(), 21);
    assert!(
        matches!(&records[20], RefusalRecord::Suppressed { principal, count: 80, .. } if principal == "alice")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_principals_do_not_share_a_budget_and_a_grant_is_never_limited() {
    let f = fixture(Mode::Allow);
    let op = OPERATIONS[0];
    for _ in 0..30 {
        let _ = f.guard.authorise("mallory", op).await;
        f.guard.authorise("alice", op).await.unwrap();
    }
    assert_eq!(
        f.audit.0.lock().unwrap().len(),
        20,
        "mallory's budget only; alice's grants write nothing"
    );
    *f.engine.0.lock().unwrap() = Mode::Deny;
    let _ = f.guard.authorise("bob", op).await;
    assert_eq!(
        f.audit.0.lock().unwrap().len(),
        21,
        "bob has his own budget"
    );
}

#[test]
fn the_limiter_reports_the_closed_window_of_a_key_at_its_next_refusal() {
    let limiter = RefusalLimiter::new(2, 60);
    assert_eq!(limiter.note("t", "p", "a", 0), (true, None));
    assert_eq!(limiter.note("t", "p", "a", 1), (true, None));
    assert_eq!(limiter.note("t", "p", "a", 2), (false, None));
    assert_eq!(limiter.note("t", "p", "a", 3), (false, None));
    assert_eq!(limiter.note("t", "p", "a", 61), (true, Some(2)));
    assert_eq!(limiter.note("t", "q", "a", 61), (true, None));
}

#[test]
fn an_expired_or_revoked_override_does_not_suppress_a_conflict_an_active_one_does() {
    let record = |user: &str| SodOverride {
        rule_id: "r1".into(),
        user: user.into(),
        scope: None,
        reason: "cover".into(),
        mitigating_control: "review".into(),
        granted_by: "admin".into(),
        granted_at: 0,
    };
    let now = 1_800_000_000;
    let stored = |user: &str, expires_at: i64| StoredOverride {
        seq: 1,
        record: record(user),
        expires_at,
        revoked: None,
    };
    let active = stored("u_active", now + 100);
    let expired = stored("u_expired", now - 1);
    assert!(active.is_active(now));
    assert!(
        !expired.is_active(now),
        "an override that expired one second ago covers nothing"
    );
    // The guard's filter keeps only what covers something at `now`.
    let kept: Vec<_> = [active, expired]
        .into_iter()
        .filter(|s| s.is_active(now))
        .map(|s| s.record.user)
        .collect();
    assert_eq!(kept, ["u_active"]);
}
