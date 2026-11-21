//! The cases every backend must pass. A backend supplies an [`Env`]; `cases!` turns each case
//! into a `#[tokio::test]` that builds a fresh one.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod recording;

use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::{Coordinator, RateLimit};
use spot_core::ManualClock;
use spot_postgres::testing::TestDb;
use spot_throttle_coordinator::{
    Attempt, CoordinatorThrottle, Kind, SaltRing, SourceAddr, ThrottleError, ThrottlePolicy,
    ThrottleSalt, Verdict,
};

pub use recording::{Failing, Handle, Recording};

pub const HOUR: Duration = Duration::from_secs(3600);

/// Several nodes on one store, and a way to let time pass.
pub struct Env {
    /// One coordinator handle per node, all on the same store.
    pub nodes: Vec<Arc<dyn Coordinator>>,
    /// Set for backends whose time is a `ManualClock`; otherwise `pass` sleeps.
    pub clock: Option<Arc<ManualClock>>,
    /// The scratch database a backend needs one for. Held here so that it is dropped — and so
    /// removed, leaving no database behind — when the case ends, rather than leaked.
    pub db: Option<TestDb>,
}

impl Env {
    pub async fn pass(&self, by: Duration) {
        match &self.clock {
            Some(clock) => clock.advance(by),
            None => tokio::time::sleep(by).await,
        }
    }
}

pub fn limit(capacity: u64, per: Duration) -> RateLimit {
    RateLimit { capacity, per }
}

pub fn policy(account: u64, source: u64, destination: u64, tenant: u64) -> ThrottlePolicy {
    ThrottlePolicy::new(
        limit(account, HOUR),
        limit(source, HOUR),
        limit(destination, HOUR),
        limit(tenant, HOUR),
    )
    .unwrap()
}

pub fn salt(byte: u8) -> ThrottleSalt {
    ThrottleSalt::new(&[byte; 32]).unwrap()
}

pub fn throttle(node: &Arc<dyn Coordinator>, policy: ThrottlePolicy) -> CoordinatorThrottle {
    // Every case runs through the ring, which is how a front door builds the throttle: one salt,
    // no previous one to check (WP-IAM-11.T12).
    CoordinatorThrottle::with_ring(Arc::clone(node), SaltRing::new(salt(7)), policy)
}

pub fn sign_in<'a>(account: &'a str, source: &'a str) -> Attempt<'a> {
    Attempt {
        kind: Kind::SignIn,
        account: Some(account),
        source: SourceAddr::new(source),
        destination: None,
        tenant: "t1",
    }
}

pub async fn admit(throttle: &CoordinatorThrottle, attempt: &Attempt<'_>) -> Verdict {
    throttle.admit(attempt).await.unwrap()
}

pub fn allowed(verdict: Verdict) -> bool {
    verdict.is_allowed()
}

// ---------------------------------------------------------------- the cases

pub async fn allow_then_deny_at_the_limit(env: Env) {
    let throttle = throttle(&env.nodes[0], policy(3, 100, 100, 1000));
    let attempt = sign_in("alice", "10.0.0.1");
    for _ in 0..3 {
        assert_eq!(admit(&throttle, &attempt).await, Verdict::Allow);
    }
    let Verdict::Deny { retry_after } = admit(&throttle, &attempt).await else {
        panic!("the fourth attempt must be denied");
    };
    assert!(retry_after > Duration::ZERO);
    assert_eq!(
        Verdict::Deny { retry_after }
            .require_allowed()
            .unwrap_err()
            .code(),
        "throttle_coordinator.rate_limited"
    );
}

/// A denial by the source charges neither the account, the destination nor the tenant.
pub async fn a_denial_leaves_the_later_buckets_uncharged(env: Env) {
    // Source: 2. Account: 3. A third attempt from S1 is denied at the source and must not
    // charge alice: from S2 she then has exactly one attempt left after S1's two.
    let throttle = throttle(&env.nodes[0], policy(3, 2, 100, 1000));
    assert!(allowed(admit(&throttle, &sign_in("alice", "s1")).await));
    assert!(allowed(admit(&throttle, &sign_in("alice", "s1")).await));
    assert!(!allowed(admit(&throttle, &sign_in("alice", "s1")).await));
    assert!(!allowed(admit(&throttle, &sign_in("alice", "s1")).await));
    assert!(allowed(admit(&throttle, &sign_in("alice", "s2")).await));
    assert!(
        !allowed(admit(&throttle, &sign_in("alice", "s2")).await),
        "alice's own bucket (3) is spent by exactly the three attempts that passed the source"
    );
}

/// Recorded security finding regression: ten failures from one source never deny a different source.
pub async fn one_source_never_denies_another(env: Env) {
    let throttle = throttle(&env.nodes[0], policy(100, 3, 100, 1000));
    for _ in 0..10 {
        let _ = admit(&throttle, &sign_in("alice", "attacker")).await;
    }
    assert!(!allowed(
        admit(&throttle, &sign_in("alice", "attacker")).await
    ));
    assert!(allowed(admit(&throttle, &sign_in("alice", "honest")).await));
    assert!(allowed(admit(&throttle, &sign_in("bob", "honest")).await));
}

/// An empty source is one bucket per connection when there is a connection id.
pub async fn an_empty_source_is_bucketed_per_connection(env: Env) {
    let throttle = throttle(&env.nodes[0], policy(100, 2, 100, 1000));
    let attempt = |connection| Attempt {
        kind: Kind::SignIn,
        account: Some("alice"),
        source: SourceAddr::connection(connection),
        destination: None,
        tenant: "t1",
    };
    for _ in 0..2 {
        assert!(allowed(admit(&throttle, &attempt("conn-1")).await));
    }
    assert!(!allowed(admit(&throttle, &attempt("conn-1")).await));
    for _ in 0..10 {
        let _ = admit(&throttle, &attempt("conn-1")).await;
    }
    assert!(allowed(admit(&throttle, &attempt("conn-2")).await));
}

/// An empty source with no connection id is refused, and counts nothing.
pub async fn an_empty_source_without_a_connection_is_refused(env: Env) {
    let recorder = Recording::new(Arc::clone(&env.nodes[0]));
    let throttle = throttle(&recorder.handle(), policy(10, 10, 10, 10));
    for source in [SourceAddr::new(""), SourceAddr::new("   ")] {
        let attempt = Attempt {
            kind: Kind::SignIn,
            account: Some("alice"),
            source,
            destination: None,
            tenant: "t1",
        };
        let error = throttle.admit(&attempt).await.unwrap_err();
        assert_eq!(error.code(), "throttle_coordinator.bad_attempt");
    }
    assert!(
        recorder.keys().is_empty(),
        "a refused attempt counts nothing"
    );
}

/// Two nodes on one store never admit more than the limit under 200 concurrent attempts.
pub async fn two_nodes_never_admit_more_than_the_limit(env: Env) {
    let a = Arc::new(throttle(&env.nodes[0], policy(1000, 50, 1000, 1000)));
    let b = Arc::new(throttle(&env.nodes[1], policy(1000, 50, 1000, 1000)));
    let mut tasks = Vec::new();
    for i in 0..200 {
        let node = if i % 2 == 0 {
            Arc::clone(&a)
        } else {
            Arc::clone(&b)
        };
        tasks.push(tokio::spawn(async move {
            // Distinct accounts: only the shared source bucket can limit these.
            let account = format!("user-{i}");
            node.admit(&sign_in(&account, "one-source")).await.unwrap()
        }));
    }
    let mut admitted = 0;
    for task in tasks {
        if task.await.unwrap().is_allowed() {
            admitted += 1;
        }
    }
    assert_eq!(admitted, 50, "exactly the limit, from two nodes");
}

pub async fn a_bucket_refills_after_the_window(env: Env) {
    let window = if env.clock.is_some() {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(2)
    };
    let policy = ThrottlePolicy::new(
        limit(2, window),
        limit(100, window),
        limit(100, window),
        limit(1000, window),
    )
    .unwrap();
    let throttle = throttle(&env.nodes[0], policy);
    let attempt = sign_in("alice", "s1");
    assert!(allowed(admit(&throttle, &attempt).await));
    assert!(allowed(admit(&throttle, &attempt).await));
    assert!(!allowed(admit(&throttle, &attempt).await));
    env.pass(window + Duration::from_millis(100)).await;
    assert!(allowed(admit(&throttle, &attempt).await));
}

/// Known and unknown names: same verdicts, same key shapes, same number of takes.
pub async fn unknown_and_known_names_look_the_same(env: Env) {
    let run = |name: &'static str| {
        let node = Arc::clone(&env.nodes[0]);
        async move {
            let recorder = Recording::new(node);
            let throttle =
                CoordinatorThrottle::new(recorder.handle(), salt(7), policy(3, 100, 100, 1000));
            let mut verdicts = Vec::new();
            for _ in 0..5 {
                verdicts.push(matches!(
                    admit(&throttle, &sign_in(name, "s-shape")).await,
                    Verdict::Allow
                ));
            }
            (verdicts, recorder.shapes())
        }
    };
    let (known_verdicts, known_shapes) = run("alice").await;
    // The second run is a name nobody has, in another tenant so the buckets start empty.
    let recorder = Recording::new(Arc::clone(&env.nodes[0]));
    let throttle = CoordinatorThrottle::new(recorder.handle(), salt(7), policy(3, 100, 100, 1000));
    let mut unknown_verdicts = Vec::new();
    for _ in 0..5 {
        let attempt = Attempt {
            tenant: "t2",
            ..sign_in("no-such-user-9f3", "s-shape")
        };
        unknown_verdicts.push(matches!(admit(&throttle, &attempt).await, Verdict::Allow));
    }
    assert_eq!(known_verdicts, unknown_verdicts);
    assert_eq!(known_verdicts, vec![true, true, true, false, false]);
    let normalise = |shapes: Vec<String>| -> Vec<String> {
        shapes.into_iter().map(|s| s.replace("t2", "t1")).collect()
    };
    assert_eq!(known_shapes, normalise(recorder.shapes()));
}

/// No raw account, address, e-mail or phone ever reaches the coordinator.
pub async fn raw_values_never_reach_the_coordinator(env: Env) {
    let recorder = Recording::new(Arc::clone(&env.nodes[0]));
    let throttle = CoordinatorThrottle::new(recorder.handle(), salt(7), policy(10, 10, 10, 10));
    let attempt = Attempt {
        kind: Kind::CodeRequest,
        account: Some("Alice.Smith"),
        source: SourceAddr::new("203.0.113.77"),
        destination: Some("Victim@Example.com"),
        tenant: "t1",
    };
    assert!(allowed(admit(&throttle, &attempt).await));
    let phone = Attempt {
        destination: Some("+923001234567"),
        source: SourceAddr::connection("conn-secret-id"),
        ..attempt
    };
    assert!(allowed(admit(&throttle, &phone).await));
    let keys = recorder.keys();
    assert_eq!(keys.len(), 8, "four buckets per attempt");
    for key in &keys {
        let lower = key.to_lowercase();
        for raw in [
            "alice",
            "smith",
            "203.0.113",
            "victim",
            "example",
            "923001234567",
            "conn-secret",
        ] {
            assert!(!lower.contains(raw), "{key} leaks {raw}");
        }
        let parts: Vec<&str> = key.split(':').collect();
        assert_eq!(parts.len(), 4, "{key}");
        assert_eq!(parts[0], "thr");
        assert_eq!(parts[1], "t1");
        assert!(["source", "destination", "account", "tenant"].contains(&parts[2]));
        assert_eq!(parts[3].len(), 64);
        assert!(parts[3].bytes().all(|b| b.is_ascii_hexdigit()));
    }
    let dimensions: Vec<String> = keys
        .iter()
        .take(4)
        .map(|k| k.split(':').nth(2).unwrap().to_owned())
        .collect();
    assert_eq!(dimensions, ["source", "destination", "account", "tenant"]);
}

/// Case variants of one e-mail or name share a bucket; different salts and tenants do not.
pub async fn buckets_are_keyed_by_salt_and_tenant(env: Env) {
    let node = &env.nodes[0];
    let throttle = throttle(node, policy(2, 100, 100, 1000));
    let a = sign_in("Alice", "s1");
    let b = sign_in("aLICE", "s2");
    assert!(allowed(admit(&throttle, &a).await));
    assert!(allowed(admit(&throttle, &b).await));
    assert!(
        !allowed(admit(&throttle, &a).await),
        "case variants share a bucket"
    );

    let other_tenant = Attempt { tenant: "t9", ..a };
    assert!(
        allowed(admit(&throttle, &other_tenant).await),
        "tenants are separate"
    );

    let other_salt = CoordinatorThrottle::new(Arc::clone(node), salt(8), policy(2, 100, 100, 1000));
    assert!(
        allowed(admit(&other_salt, &a).await),
        "a rotated salt starts afresh"
    );
}

/// A sign-in flood and a recovery flood for one name do not deny each other; the destination
/// bucket is shared across kinds.
pub async fn kinds_have_their_own_account_bucket_but_share_a_destination(env: Env) {
    let throttle = throttle(&env.nodes[0], policy(2, 100, 3, 1000));
    for _ in 0..2 {
        assert!(allowed(admit(&throttle, &sign_in("alice", "s1")).await));
    }
    assert!(!allowed(admit(&throttle, &sign_in("alice", "s1")).await));
    let recovery = |source| Attempt {
        kind: Kind::Recovery,
        account: Some("alice"),
        source: SourceAddr::new(source),
        destination: Some("alice@example.com"),
        tenant: "t1",
    };
    assert!(allowed(admit(&throttle, &recovery("s2")).await));
    let code = |source| Attempt {
        kind: Kind::CodeRequest,
        account: None,
        source: SourceAddr::new(source),
        destination: Some("ALICE@example.com"),
        tenant: "t1",
    };
    assert!(allowed(admit(&throttle, &code("s3")).await));
    assert!(allowed(admit(&throttle, &code("s4")).await));
    assert!(
        !allowed(admit(&throttle, &code("s5")).await),
        "three messages to one destination"
    );
}

pub async fn the_tenant_ceiling_denies_everyone_in_the_tenant(env: Env) {
    let throttle = throttle(&env.nodes[0], policy(100, 3, 100, 3));
    for i in 0..3 {
        let name = format!("u{i}");
        let source = format!("s{i}");
        assert!(allowed(admit(&throttle, &sign_in(&name, &source)).await));
    }
    assert!(!allowed(admit(&throttle, &sign_in("u9", "s9")).await));
    let other = Attempt {
        tenant: "t2",
        ..sign_in("u9", "s9")
    };
    assert!(allowed(admit(&throttle, &other).await));
}

/// A failing coordinator refuses, whichever take fails, and never answers Allow.
pub async fn a_failing_coordinator_refuses(env: Env) {
    let attempt = Attempt {
        kind: Kind::Recovery,
        account: Some("alice"),
        source: SourceAddr::new("s1"),
        destination: Some("alice@example.com"),
        tenant: "t1",
    };
    for fail_on in 1..=4 {
        let failing = Failing::new(Arc::clone(&env.nodes[0]), fail_on);
        let throttle = CoordinatorThrottle::new(failing.handle(), salt(7), policy(10, 10, 10, 10));
        let result = throttle.admit(&attempt).await;
        match result {
            Err(ThrottleError::Unavailable { cause }) => {
                assert_eq!(cause, "coordinator.invalid_argument");
            }
            other => panic!("take {fail_on} failed but the answer was {other:?}"),
        }
        assert_eq!(
            result.unwrap_err().code(),
            "throttle_coordinator.unavailable"
        );
    }
}

/// A rotation keeps an exhausted bucket denied until the previous window has refilled, and a
/// name the previous window never saw is unharmed (WP-IAM-11.T12).
pub async fn a_rotated_ring_keeps_an_exhausted_limit_denied(env: Env) {
    let window = if env.clock.is_some() {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(2)
    };
    let policy = ThrottlePolicy::new(
        limit(2, window),
        limit(100, window),
        limit(100, window),
        limit(1000, window),
    )
    .unwrap();
    let before =
        CoordinatorThrottle::with_ring(Arc::clone(&env.nodes[0]), SaltRing::new(salt(7)), policy);
    assert!(allowed(admit(&before, &sign_in("alice", "s1")).await));
    assert!(allowed(admit(&before, &sign_in("alice", "s2")).await));
    assert!(
        !allowed(admit(&before, &sign_in("alice", "s3")).await),
        "alice's account bucket is spent under the old salt"
    );

    let after = CoordinatorThrottle::with_ring(
        Arc::clone(&env.nodes[0]),
        SaltRing::new(salt(7)).rotate(salt(8)),
        policy,
    );
    assert!(
        !allowed(admit(&after, &sign_in("alice", "s4")).await),
        "a rotation must not hand the exhausted account a fresh bucket"
    );
    assert!(
        allowed(admit(&after, &sign_in("bob", "s4")).await),
        "a name the previous window never saw is not denied by the rotation"
    );
    env.pass(window + Duration::from_millis(100)).await;
    assert!(
        allowed(admit(&after, &sign_in("alice", "s5")).await),
        "once the previous window refills, the rotation is over"
    );
}

/// One `#[tokio::test]` per case. `$env` is an `async fn() -> Option<Env>`; `None` skips.
macro_rules! cases {
    ($env:path) => {
        $crate::suite::cases!(@one $env, allow_then_deny_at_the_limit);
        $crate::suite::cases!(@one $env, a_denial_leaves_the_later_buckets_uncharged);
        $crate::suite::cases!(@one $env, one_source_never_denies_another);
        $crate::suite::cases!(@one $env, an_empty_source_is_bucketed_per_connection);
        $crate::suite::cases!(@one $env, an_empty_source_without_a_connection_is_refused);
        $crate::suite::cases!(@one $env, two_nodes_never_admit_more_than_the_limit);
        $crate::suite::cases!(@one $env, a_bucket_refills_after_the_window);
        $crate::suite::cases!(@one $env, unknown_and_known_names_look_the_same);
        $crate::suite::cases!(@one $env, raw_values_never_reach_the_coordinator);
        $crate::suite::cases!(@one $env, buckets_are_keyed_by_salt_and_tenant);
        $crate::suite::cases!(@one $env, kinds_have_their_own_account_bucket_but_share_a_destination);
        $crate::suite::cases!(@one $env, the_tenant_ceiling_denies_everyone_in_the_tenant);
        $crate::suite::cases!(@one $env, a_failing_coordinator_refuses);
        $crate::suite::cases!(@one $env, a_rotated_ring_keeps_an_exhausted_limit_denied);
    };
    (@one $env:path, $case:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $case() {
            if let Some(env) = $env().await {
                $crate::suite::$case(env).await;
            }
        }
    };
}
pub(crate) use cases;
