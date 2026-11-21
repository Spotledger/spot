//! The account spelling: one fold, counted and returned.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;
use spot_consumer_admission::{
    AdmissionPolicy, BAD_REQUEST, ConsumerAdmission, Purpose, RATE_LIMITED, Request, fold_account,
};
use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId, RateLimit};
use spot_core::ManualClock;
use spot_throttle_coordinator::{
    CoordinatorThrottle, MAX_VALUE_BYTES, SaltRing, SourceAddr, ThrottlePolicy, ThrottleSalt,
};
use time::OffsetDateTime;

const HOUR: Duration = Duration::from_secs(3600);
const N: u64 = 4;

fn limit(capacity: u64) -> RateLimit {
    RateLimit {
        capacity,
        per: HOUR,
    }
}

async fn admission() -> ConsumerAdmission {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let coordinator: Arc<dyn Coordinator> = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        clock as Arc<dyn spot_core::Clock>,
    )
    .await
    .unwrap();
    let policy = ThrottlePolicy::new(
        limit(N),
        limit(1_000_000),
        limit(1_000_000),
        limit(1_000_000),
    )
    .unwrap();
    let throttle = Arc::new(CoordinatorThrottle::with_ring(
        coordinator,
        SaltRing::new(ThrottleSalt::new(&[7_u8; 32]).unwrap()),
        policy,
    ));
    ConsumerAdmission::new(throttle, AdmissionPolicy::new(true, 1, None).unwrap()).unwrap()
}

fn sign_in(account: &str) -> Request<'_> {
    Request {
        purpose: Purpose::SignIn,
        tenant: "acme",
        account: Some(account),
        destination: None,
        source: SourceAddr::new("10.0.0.1"),
    }
}

#[tokio::test]
async fn every_spelling_of_a_name_is_one_bucket() {
    let admission = admission().await;
    let spellings = ["Alice", "ALICE", " alice ", "alice"];
    for i in 0..N {
        let spelling = spellings[usize::try_from(i).unwrap() % spellings.len()];
        admission
            .admit(&sign_in(spelling))
            .await
            .expect("within the limit");
    }
    let mut refusals = Vec::new();
    for spelling in spellings {
        let error = admission
            .admit(&sign_in(spelling))
            .await
            .expect_err("the shared bucket is spent");
        assert_eq!(error.code(), RATE_LIMITED);
        refusals.push(error.retry_after());
    }
    assert!(refusals.windows(2).all(|w| w[0] == w[1]), "{refusals:?}");
}

#[tokio::test]
async fn the_admission_carries_the_folded_account() {
    let admission = admission().await;
    for raw in ["Alice", " ALICE\t", "alice", "ÉLODIE"] {
        let admitted = admission.admit(&sign_in(raw)).await.unwrap();
        assert_eq!(
            admitted.account(),
            Some(fold_account(raw).unwrap().as_str())
        );
        let debug = format!("{admitted:?}");
        assert!(!debug.to_lowercase().contains("alice"), "{debug}");
        assert!(!debug.to_lowercase().contains("lodie"), "{debug}");
    }
}

#[tokio::test]
async fn a_refused_name_spends_nothing() {
    let admission = admission().await;
    let long = "a".repeat(MAX_VALUE_BYTES + 1);
    for bad in [
        "",
        "   ",
        "al\u{0}ice",
        "al\nice",
        "a\u{7f}b",
        long.as_str(),
    ] {
        let error = admission.admit(&sign_in(bad)).await.expect_err("refused");
        assert_eq!(error.code(), BAD_REQUEST, "{bad:?}");
    }
    // The whole allowance is still there.
    for _ in 0..N {
        admission.admit(&sign_in("alice")).await.unwrap();
    }
    assert!(admission.admit(&sign_in("alice")).await.is_err());
}

#[test]
fn folding_trims_lower_cases_and_is_stable() {
    assert_eq!(fold_account(" Alice ").unwrap(), "alice");
    // Capital sigma and the Turkish dotted capital I fold to something a second fold keeps.
    for raw in ["ΟΔΥΣΣΕΥΣ", "İSTANBUL", "ǅ", "ẞ"] {
        let once = fold_account(raw).unwrap();
        assert_eq!(fold_account(&once).unwrap(), once, "{raw}");
    }
    // No other normalisation: dots and plus tags are part of the name.
    assert_ne!(fold_account("a.b").unwrap(), fold_account("ab").unwrap());
    assert_ne!(fold_account("a+x").unwrap(), fold_account("a").unwrap());
}

#[test]
fn folding_that_lengthens_past_the_limit_refuses() {
    // U+0130 lower-cases to three bytes from two.
    let name = "\u{130}".repeat(MAX_VALUE_BYTES / 2);
    assert!(name.len() <= MAX_VALUE_BYTES);
    assert_eq!(fold_account(&name).unwrap_err().code(), BAD_REQUEST);
}

proptest! {
    #[test]
    fn fold_account_is_total_and_idempotent(raw in ".{0,300}") {
        if let Ok(once) = fold_account(&raw) {
            prop_assert_eq!(fold_account(&once).unwrap(), once);
        }
    }
}
