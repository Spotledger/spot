//! The [`SodStore`] contract suite (feature `testing`): every backend must pass [`run_all`].
//! Async and runtime-free; each case uses a tenant of its own.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;

use spot_tenancy::TenantId;

use crate::override_sod::SodOverride;
use crate::store::{MAX_PAGE, MAX_TEXT_BYTES, SodStore, SodStoreError};

/// Two rules in the loader's TOML shape.
pub const RULES_V1: &str = r#"
[[rules]]
id = "sod.vendor-master.payments"
title = "Maintain suppliers vs approve payments"
description = "Who maintains the supplier master must not approve the payments."
severity = "critical"
scope = "same_scope"
[[rules.sides]]
duties = ["Maintain supplier master"]
[[rules.sides]]
duties = ["Approve payment run"]

[[rules]]
id = "sod.purchase-order.approval"
title = "Create vs approve purchase orders"
description = "Who raises a purchase order must not approve it."
severity = "high"
scope = "same_scope"
[[rules.sides]]
duties = ["Create purchase order"]
[[rules.sides]]
duties = ["Approve purchase order"]
"#;

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A tenant no other case (or earlier run) used.
#[must_use]
pub fn fresh_tenant() -> TenantId {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let mut entropy = [0_u8; 10];
    entropy[..8].copy_from_slice(&n.to_be_bytes());
    entropy[8] = 0x5b;
    TenantId::new_v7(1_700_000_000_000 + n, entropy)
}

/// Polls both futures on the calling task, interleaved, until both are done.
pub async fn join2<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = Box::pin(a);
    let mut b = Box::pin(b);
    let (mut ra, mut rb) = (None, None);
    std::future::poll_fn(|cx| {
        if ra.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            ra = Some(v);
        }
        if rb.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            rb = Some(v);
        }
        if ra.is_some() && rb.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (ra.unwrap(), rb.unwrap())
}

struct ThreadWaker(std::thread::Thread);

impl std::task::Wake for ThreadWaker {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives a future to completion on the calling thread, for tests that have no runtime.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = std::task::Waker::from(std::sync::Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

/// An override of the first rule for `user`, granted by `admin` at 1000.
#[must_use]
pub fn grant(user: &str, scope: Option<&str>) -> SodOverride {
    SodOverride {
        rule_id: "sod.vendor-master.payments".to_string(),
        user: user.to_string(),
        scope: scope.map(str::to_string),
        reason: "small team, interim".to_string(),
        mitigating_control: "monthly director review".to_string(),
        granted_by: "admin".to_string(),
        granted_at: 1000,
    }
}

/// Runs every case. `make` returns a store over the same backend each call.
pub async fn run_all<S: SodStore>(make: impl Fn() -> S) {
    rule_set_versions(&make()).await;
    invalid_rule_set_is_refused(&make()).await;
    racing_rule_set_writes_have_one_winner(&make()).await;
    overrides_need_a_rule_and_complete_records(&make()).await;
    one_active_override_per_rule_user_scope(&make()).await;
    expiry_and_revocation(&make()).await;
    overrides_are_listed_in_order(&make()).await;
    tenant_isolation(&make()).await;
}

async fn rule_set_versions<S: SodStore>(s: &S) {
    let t = fresh_tenant();
    assert_eq!(s.rule_set(&t).await.unwrap(), None);
    assert_eq!(
        s.put_rule_set(&t, RULES_V1, 0, "admin", 10).await.unwrap(),
        1
    );
    let got = s.rule_set(&t).await.unwrap().unwrap();
    assert_eq!(got.version, 1);
    assert_eq!(got.rules, crate::RuleSet::from_toml(RULES_V1).unwrap());
    assert_eq!((got.set_by.as_str(), got.set_at), ("admin", 10));
    // A stale writer is refused; the right version is accepted.
    assert_eq!(
        s.put_rule_set(&t, RULES_V1, 0, "admin", 11)
            .await
            .unwrap_err(),
        SodStoreError::VersionConflict
    );
    assert_eq!(s.put_rule_set(&t, "", 1, "admin", 12).await.unwrap(), 2);
    assert!(
        s.rule_set(&t)
            .await
            .unwrap()
            .unwrap()
            .rules
            .rules
            .is_empty()
    );
}

async fn invalid_rule_set_is_refused<S: SodStore>(s: &S) {
    let t = fresh_tenant();
    let one_side = RULES_V1.replace("[[rules.sides]]\nduties = [\"Approve payment run\"]\n", "");
    let e = s
        .put_rule_set(&t, &one_side, 0, "admin", 10)
        .await
        .unwrap_err();
    assert_eq!(e.code(), "sod.side_count");
    assert_eq!(
        s.put_rule_set(&t, "not toml [[", 0, "admin", 10)
            .await
            .unwrap_err()
            .code(),
        "sod.parse_failed"
    );
    assert_eq!(s.rule_set(&t).await.unwrap(), None, "nothing was stored");
    assert!(matches!(
        s.put_rule_set(&t, RULES_V1, 0, "  ", 10).await.unwrap_err(),
        SodStoreError::InputInvalid(_)
    ));
}

async fn racing_rule_set_writes_have_one_winner<S: SodStore>(s: &S) {
    let t = fresh_tenant();
    let (a, b) = join2(
        s.put_rule_set(&t, RULES_V1, 0, "alice", 10),
        s.put_rule_set(&t, RULES_V1, 0, "bob", 10),
    )
    .await;
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "{a:?} / {b:?}"
    );
    assert_eq!(s.rule_set(&t).await.unwrap().unwrap().version, 1);
}

async fn overrides_need_a_rule_and_complete_records<S: SodStore>(s: &S) {
    let t = fresh_tenant();
    // No rule set yet: refused (fail closed).
    assert!(matches!(
        s.grant_override(&t, grant("zoe", None), 5000, 1000)
            .await
            .unwrap_err(),
        SodStoreError::InputInvalid(_)
    ));
    s.put_rule_set(&t, RULES_V1, 0, "admin", 10).await.unwrap();
    let mut unknown = grant("zoe", None);
    unknown.rule_id = "no-such-rule".to_string();
    let mut blank_reason = grant("zoe", None);
    blank_reason.reason = "  ".to_string();
    let mut blank_control = grant("zoe", None);
    blank_control.mitigating_control = String::new();
    let mut self_grant = grant("admin", None);
    self_grant.granted_by = "admin".to_string();
    let mut long = grant("zoe", None);
    long.reason = "r".repeat(MAX_TEXT_BYTES + 1);
    for bad in [unknown, blank_reason, blank_control, self_grant, long] {
        assert!(matches!(
            s.grant_override(&t, bad, 5000, 1000).await.unwrap_err(),
            SodStoreError::InputInvalid(_)
        ));
    }
    // Already expired.
    assert!(matches!(
        s.grant_override(&t, grant("zoe", None), 1000, 1000)
            .await
            .unwrap_err(),
        SodStoreError::InputInvalid(_)
    ));
    assert!(
        s.list_overrides(&t, 0, 10).await.unwrap().is_empty(),
        "nothing was stored"
    );
    let seq = s
        .grant_override(&t, grant("zoe", None), 5000, 1000)
        .await
        .unwrap();
    assert!(seq > 0);
    let got = s.list_overrides(&t, 0, 10).await.unwrap();
    assert_eq!(
        got[0].record,
        grant("zoe", None),
        "stored exactly as granted"
    );
    assert_eq!(got[0].expires_at, 5000);
}

async fn one_active_override_per_rule_user_scope<S: SodStore>(s: &S) {
    let t = fresh_tenant();
    s.put_rule_set(&t, RULES_V1, 0, "admin", 10).await.unwrap();
    s.grant_override(&t, grant("zoe", Some("eu")), 5000, 1000)
        .await
        .unwrap();
    assert_eq!(
        s.grant_override(&t, grant("zoe", Some("eu")), 6000, 1001)
            .await
            .unwrap_err(),
        SodStoreError::AlreadyActive
    );
    // A different scope, no scope, another user: each its own record.
    s.grant_override(&t, grant("zoe", Some("us")), 5000, 1000)
        .await
        .unwrap();
    s.grant_override(&t, grant("zoe", None), 5000, 1000)
        .await
        .unwrap();
    s.grant_override(&t, grant("yan", Some("eu")), 5000, 1000)
        .await
        .unwrap();
    // Racing grants of one triple: one winner.
    let (a, b) = join2(
        s.grant_override(&t, grant("xi", None), 5000, 1000),
        s.grant_override(&t, grant("xi", None), 5000, 1000),
    )
    .await;
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "{a:?} / {b:?}"
    );
    assert_eq!(s.active_overrides(&t, 1500).await.unwrap().len(), 5);
}

async fn expiry_and_revocation<S: SodStore>(s: &S) {
    let t = fresh_tenant();
    s.put_rule_set(&t, RULES_V1, 0, "admin", 10).await.unwrap();
    let first = s
        .grant_override(&t, grant("zoe", None), 2000, 1000)
        .await
        .unwrap();
    assert_eq!(s.active_overrides(&t, 1999).await.unwrap().len(), 1);
    assert!(
        s.active_overrides(&t, 2000).await.unwrap().is_empty(),
        "expired at its expiry"
    );
    // After expiry a successor may be recorded; the first stays as evidence.
    let second = s
        .grant_override(&t, grant("zoe", None), 9000, 2500)
        .await
        .unwrap();
    assert!(second > first);
    s.revoke_override(&t, second, "auditor", "control lapsed", 3000)
        .await
        .unwrap();
    assert!(s.active_overrides(&t, 3001).await.unwrap().is_empty());
    assert_eq!(
        s.revoke_override(&t, second, "auditor", "again", 3001)
            .await
            .unwrap_err(),
        SodStoreError::AlreadyRevoked
    );
    assert_eq!(
        s.revoke_override(&t, 999, "auditor", "x", 3001)
            .await
            .unwrap_err(),
        SodStoreError::NotFound
    );
    assert!(matches!(
        s.revoke_override(&t, first, "auditor", " ", 3001)
            .await
            .unwrap_err(),
        SodStoreError::InputInvalid(_)
    ));
    let all = s.list_overrides(&t, 0, MAX_PAGE).await.unwrap();
    assert_eq!(all.len(), 2, "revoked and expired overrides stay listed");
    assert_eq!(all[0].revoked, None);
    let rev = all[1].revoked.as_ref().unwrap();
    assert_eq!(
        (rev.by.as_str(), rev.reason.as_str(), rev.at),
        ("auditor", "control lapsed", 3000)
    );
    assert_eq!(
        all[1].record,
        grant("zoe", None),
        "revocation left the record untouched"
    );
    // A revoked triple may be granted again.
    s.grant_override(&t, grant("zoe", None), 9000, 3100)
        .await
        .unwrap();
}

async fn overrides_are_listed_in_order<S: SodStore>(s: &S) {
    let t = fresh_tenant();
    s.put_rule_set(&t, RULES_V1, 0, "admin", 10).await.unwrap();
    for u in ["a", "b", "c"] {
        s.grant_override(&t, grant(u, None), 5000, 1000)
            .await
            .unwrap();
    }
    let all = s.list_overrides(&t, 0, MAX_PAGE).await.unwrap();
    assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(
        s.list_overrides(&t, all[0].seq, MAX_PAGE).await.unwrap(),
        all[1..]
    );
    assert_eq!(s.list_overrides(&t, 0, 2).await.unwrap().len(), 2);
    assert!(matches!(
        s.list_overrides(&t, 0, 0).await.unwrap_err(),
        SodStoreError::InputInvalid(_)
    ));
    assert!(matches!(
        s.list_overrides(&t, 0, MAX_PAGE + 1).await.unwrap_err(),
        SodStoreError::InputInvalid(_)
    ));
}

async fn tenant_isolation<S: SodStore>(s: &S) {
    let (a, b) = (fresh_tenant(), fresh_tenant());
    s.put_rule_set(&a, RULES_V1, 0, "admin", 10).await.unwrap();
    let seq = s
        .grant_override(&a, grant("zoe", None), 5000, 1000)
        .await
        .unwrap();
    assert_eq!(s.rule_set(&b).await.unwrap(), None);
    assert!(s.active_overrides(&b, 1500).await.unwrap().is_empty());
    assert!(s.list_overrides(&b, 0, 10).await.unwrap().is_empty());
    assert_eq!(
        s.revoke_override(&b, seq, "auditor", "x", 1500)
            .await
            .unwrap_err(),
        SodStoreError::NotFound
    );
    // b has no rule set, so it cannot record an override for a's rule either.
    assert!(matches!(
        s.grant_override(&b, grant("zoe", None), 5000, 1000)
            .await
            .unwrap_err(),
        SodStoreError::InputInvalid(_)
    ));
    assert_eq!(s.active_overrides(&a, 1500).await.unwrap().len(), 1);
    // The same version number is independent per tenant.
    assert_eq!(
        s.put_rule_set(&b, RULES_V1, 0, "admin", 10).await.unwrap(),
        1
    );
}
