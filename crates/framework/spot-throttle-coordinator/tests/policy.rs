//! Policy, salt and attempt validation: nothing here needs a coordinator.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId, RateLimit};
use spot_core::{Clock, ManualClock};
use spot_throttle_coordinator::{
    Attempt, CoordinatorThrottle, Kind, SourceAddr, ThrottlePolicy, ThrottleSalt,
};
use time::OffsetDateTime;

fn limit(capacity: u64, secs: u64) -> RateLimit {
    RateLimit {
        capacity,
        per: Duration::from_secs(secs),
    }
}

#[test]
fn every_limit_must_be_non_zero() {
    let ok = limit(5, 60);
    for bad in [limit(0, 60), limit(5, 0)] {
        for position in 0..4 {
            let mut limits = [ok; 4];
            limits[position] = bad;
            let error =
                ThrottlePolicy::new(limits[0], limits[1], limits[2], limits[3]).unwrap_err();
            assert_eq!(error.code(), "throttle_coordinator.bad_policy");
        }
    }
}

#[test]
fn the_tenant_ceiling_must_be_at_least_the_source_rate() {
    // 10 per minute per source against 5 per minute per tenant: refused.
    let refused = ThrottlePolicy::new(limit(5, 60), limit(10, 60), limit(5, 60), limit(5, 60));
    assert_eq!(
        refused.unwrap_err().code(),
        "throttle_coordinator.bad_policy"
    );
    // The same rate over a different window is compared as a rate: 60 per hour = 1 per minute.
    assert!(ThrottlePolicy::new(limit(5, 60), limit(1, 60), limit(5, 60), limit(60, 3600)).is_ok());
    assert!(
        ThrottlePolicy::new(limit(5, 60), limit(2, 60), limit(5, 60), limit(60, 3600)).is_err()
    );
    // Equal is fine.
    assert!(ThrottlePolicy::new(limit(5, 60), limit(10, 60), limit(5, 60), limit(10, 60)).is_ok());
}

#[test]
fn a_short_salt_is_refused_and_a_salt_never_prints() {
    assert_eq!(
        ThrottleSalt::new(&[1; 15]).unwrap_err().code(),
        "throttle_coordinator.bad_policy"
    );
    let salt = ThrottleSalt::new(b"0123456789abcdef-secret-bytes").unwrap();
    let shown = format!("{salt:?}");
    assert!(
        !shown.contains("secret") && !shown.contains("0123"),
        "{shown}"
    );
}

#[tokio::test]
async fn malformed_attempts_are_refused_before_anything_is_counted() {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let coordinator = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("n").unwrap(),
        Arc::clone(&clock) as Arc<dyn Clock>,
    )
    .await
    .unwrap();
    let node = Arc::clone(&coordinator) as Arc<dyn Coordinator>;
    let policy =
        ThrottlePolicy::new(limit(5, 60), limit(50, 60), limit(50, 60), limit(50, 60)).unwrap();
    let throttle = CoordinatorThrottle::new(node, ThrottleSalt::new(&[9; 32]).unwrap(), policy);

    let base = Attempt {
        kind: Kind::SignIn,
        account: Some("alice"),
        source: SourceAddr::new("s1"),
        destination: None,
        tenant: "t1",
    };
    let long = "x".repeat(2000);
    let bad = [
        Attempt { tenant: "", ..base },
        Attempt {
            tenant: "a:b",
            ..base
        },
        Attempt {
            tenant: &long,
            ..base
        },
        Attempt {
            account: None,
            ..base
        },
        Attempt {
            account: Some(" "),
            ..base
        },
        Attempt {
            account: Some(&long),
            ..base
        },
        Attempt {
            kind: Kind::CodeRequest,
            destination: None,
            ..base
        },
        Attempt {
            kind: Kind::Recovery,
            account: None,
            destination: None,
            ..base
        },
        Attempt {
            source: SourceAddr::new(&long),
            ..base
        },
    ];
    for attempt in &bad {
        let error = throttle.admit(attempt).await.unwrap_err();
        assert_eq!(
            error.code(),
            "throttle_coordinator.bad_attempt",
            "{attempt:?}"
        );
        assert!(!error.to_string().contains("alice"));
    }
    // Registration needs neither account nor destination.
    let register = Attempt {
        kind: Kind::Register,
        account: None,
        ..base
    };
    assert!(throttle.admit(&register).await.unwrap().is_allowed());
    // Nothing above was counted: the full account allowance is still there.
    for _ in 0..5 {
        assert!(throttle.admit(&base).await.unwrap().is_allowed());
    }
    assert!(!throttle.admit(&base).await.unwrap().is_allowed());
}
