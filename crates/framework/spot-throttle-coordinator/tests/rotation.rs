//! Rotation against a real coordinator: the previous window keeps denying, charges nothing of
//! the new salt, and a new account is not hurt by a rotation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId, RateLimit};
use spot_core::ManualClock;
use spot_throttle_coordinator::{
    Attempt, CoordinatorThrottle, Kind, SaltRing, SourceAddr, ThrottlePolicy, ThrottleSalt, Verdict,
};
use time::OffsetDateTime;

const HOUR: Duration = Duration::from_secs(3600);

fn salt(byte: u8) -> ThrottleSalt {
    ThrottleSalt::new(&[byte; 32]).unwrap()
}

fn limit(capacity: u64) -> RateLimit {
    RateLimit {
        capacity,
        per: HOUR,
    }
}

/// Account 2 per hour, source and tenant generous: the account bucket is the one under test.
fn policy() -> ThrottlePolicy {
    ThrottlePolicy::new(limit(2), limit(100), limit(100), limit(1000)).unwrap()
}

struct Node {
    coordinator: Arc<dyn Coordinator>,
    clock: Arc<ManualClock>,
}

async fn node() -> Node {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let coordinator = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    )
    .await
    .unwrap();
    Node {
        coordinator: coordinator as Arc<dyn Coordinator>,
        clock,
    }
}

fn sign_in<'a>(account: &'a str, source: &'a str) -> Attempt<'a> {
    Attempt {
        kind: Kind::SignIn,
        account: Some(account),
        source: SourceAddr::new(source),
        destination: None,
        tenant: "t1",
    }
}

async fn verdict(throttle: &CoordinatorThrottle, attempt: &Attempt<'_>) -> Verdict {
    throttle.admit(attempt).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotation_keeps_an_exhausted_account_denied_until_the_window_refills() {
    let node = node().await;
    let policy = policy();
    let before = CoordinatorThrottle::with_ring(
        Arc::clone(&node.coordinator),
        SaltRing::new(salt(1)),
        policy,
    );
    // Spend alice's account bucket: two attempts from two sources, so only the account denies.
    assert!(verdict(&before, &sign_in("alice", "s1")).await.is_allowed());
    assert!(verdict(&before, &sign_in("alice", "s2")).await.is_allowed());
    assert!(!verdict(&before, &sign_in("alice", "s3")).await.is_allowed());

    // Rotate: the fresh salt has an empty account bucket for alice, the previous one is spent.
    let after = CoordinatorThrottle::with_ring(
        Arc::clone(&node.coordinator),
        SaltRing::new(salt(1)).rotate(salt(2)),
        policy,
    );
    assert!(
        !verdict(&after, &sign_in("alice", "s4")).await.is_allowed(),
        "a rotation must not hand alice's exhausted limit back"
    );

    // One window of the longest bucket later the old bucket has refilled on its own.
    node.clock.advance(HOUR + Duration::from_millis(1));
    assert!(verdict(&after, &sign_in("alice", "s5")).await.is_allowed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denial_from_the_previous_window_charges_none_of_the_new_salts_buckets() {
    let node = node().await;
    let policy = policy();
    let before = CoordinatorThrottle::with_ring(
        Arc::clone(&node.coordinator),
        SaltRing::new(salt(1)),
        policy,
    );
    for source in ["s1", "s2"] {
        assert!(
            verdict(&before, &sign_in("alice", source))
                .await
                .is_allowed()
        );
    }
    let ring = SaltRing::new(salt(1)).rotate(salt(2));
    let after = CoordinatorThrottle::with_ring(Arc::clone(&node.coordinator), ring.clone(), policy);
    // Denied by the previous salt's account bucket.
    assert!(!verdict(&after, &sign_in("alice", "s3")).await.is_allowed());

    // Retiring the previous salt leaves alice's allowance under the new salt untouched: the two
    // denied attempts above did not spend it (they were refused before it was charged).
    let mut retired = ring;
    retired.retire_previous();
    let fresh = CoordinatorThrottle::with_ring(Arc::clone(&node.coordinator), retired, policy);
    assert!(verdict(&fresh, &sign_in("alice", "s4")).await.is_allowed());
    assert!(verdict(&fresh, &sign_in("alice", "s5")).await.is_allowed());
    assert!(!verdict(&fresh, &sign_in("alice", "s6")).await.is_allowed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_account_after_a_rotation_keeps_its_full_allowance() {
    let node = node().await;
    let policy = policy();
    let before = CoordinatorThrottle::with_ring(
        Arc::clone(&node.coordinator),
        SaltRing::new(salt(1)),
        policy,
    );
    for source in ["s1", "s2"] {
        assert!(
            verdict(&before, &sign_in("alice", source))
                .await
                .is_allowed()
        );
    }
    let after = CoordinatorThrottle::with_ring(
        Arc::clone(&node.coordinator),
        SaltRing::new(salt(1)).rotate(salt(2)),
        policy,
    );
    // Carol was never seen by the previous window, so it cannot deny her.
    assert!(verdict(&after, &sign_in("carol", "c1")).await.is_allowed());
    assert!(verdict(&after, &sign_in("carol", "c2")).await.is_allowed());
    assert!(!verdict(&after, &sign_in("carol", "c3")).await.is_allowed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_salt_throttle_is_a_ring_with_no_previous() {
    let node = node().await;
    let policy = policy();
    let single = CoordinatorThrottle::new(Arc::clone(&node.coordinator), salt(1), policy);
    assert!(!single.ring().has_previous());
    assert!(verdict(&single, &sign_in("alice", "s1")).await.is_allowed());
    assert!(verdict(&single, &sign_in("alice", "s2")).await.is_allowed());
    assert!(!verdict(&single, &sign_in("alice", "s3")).await.is_allowed());
    assert_eq!(single.policy().per_account(), limit(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_debug_of_a_ringed_throttle_is_redacted() {
    let node = node().await;
    let throttle = CoordinatorThrottle::with_ring(
        Arc::clone(&node.coordinator),
        SaltRing::with_previous(salt(2), salt(1)),
        policy(),
    );
    let debug = format!("{throttle:?}");
    assert!(debug.contains("ThrottleSalt(<redacted>)"), "{debug}");
    assert!(!debug.contains("01010101") && !debug.contains("02020202"));
    assert!(debug.contains("policy"));
}
