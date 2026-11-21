//! The rate limiter behind the registry: refill, burst, eviction and the memory bound, all
//! over a manual clock; the admin listing shows the override.
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use spot_functions::{FunctionRegistryBuilder, RateLimit, codes};

fn limited(
    permits: u32,
    per: Duration,
) -> (FunctionRegistryBuilder, Arc<spot_core::clock::ManualClock>) {
    let clock = manual_clock();
    let mut b = FunctionRegistryBuilder::new();
    b.clock(clock.clone());
    let (handler, _) = FakeHandler::new();
    b.register_decl(
        "app1",
        spot_functions::FunctionDecl {
            key: ok_key(),
            verbs: None,
            guest: false,
            limit: Some(RateLimit { permits, per }),
            arguments: None,
        },
        Arc::new(handler),
    );
    (b, clock)
}

fn build_limited(
    permits: u32,
    per: Duration,
) -> (
    spot_functions::FunctionRegistry,
    Arc<spot_core::clock::ManualClock>,
) {
    let (b, clock) = limited(permits, per);
    (b.build().unwrap(), clock)
}

#[tokio::test]
async fn burst_up_to_the_limit_then_rate_limited() {
    let (registry, _) = build_limited(3, Duration::from_secs(10));
    for _ in 0..3 {
        assert!(
            registry
                .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
                .await
                .is_ok()
        );
    }
    let err = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::RATE_LIMITED);
    assert!(err.retryable());
}

#[tokio::test]
async fn tokens_refill_with_elapsed_time() {
    let (registry, clock) = build_limited(1, Duration::from_secs(10));
    assert!(
        registry
            .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
            .await
            .is_ok()
    );
    assert!(
        registry
            .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
            .await
            .is_err()
    );
    clock.advance(Duration::from_secs(5));
    // Half a window: not a whole token yet (integer refill, no floats).
    assert!(
        registry
            .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
            .await
            .is_err()
    );
    clock.advance(Duration::from_secs(5));
    assert!(
        registry
            .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn separate_callers_have_separate_buckets() {
    let (registry, _) = build_limited(1, Duration::from_secs(10));
    assert!(
        registry
            .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
            .await
            .is_ok()
    );
    // A different user still has a full bucket; the anonymous bucket is shared by all guests.
    assert!(
        registry
            .dispatch(&ctx_for(user("u2"), None), &ok_key(), json_args())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn eviction_drops_idle_buckets_and_bounds_memory() {
    let (registry, clock) = build_limited(1, Duration::from_secs(10));
    for i in 0..10 {
        assert!(
            registry
                .dispatch(
                    &ctx_for(user(&format!("u{i}")), None),
                    &ok_key(),
                    json_args()
                )
                .await
                .is_ok()
        );
    }
    assert_eq!(registry.bucket_count(), 10);
    clock.advance(Duration::from_secs(30));
    assert_eq!(registry.evict_idle_buckets(Duration::from_secs(20)), 10);
    assert_eq!(registry.bucket_count(), 0);
    // An evicted caller just starts a fresh bucket.
    assert!(
        registry
            .dispatch(&ctx_for(user("u0"), None), &ok_key(), json_args())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn concurrent_dispatch_never_exceeds_the_limit() {
    let (registry, _) = build_limited(5, Duration::from_secs(60));
    let registry = Arc::new(registry);
    let mut handles = Vec::new();
    for _ in 0..20 {
        let r = registry.clone();
        handles.push(tokio::spawn(async move {
            r.dispatch(&ctx_for(user("burst"), None), &ok_key(), json_args())
                .await
        }));
    }
    let mut ok = 0usize;
    let mut limited = 0usize;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => ok += 1,
            Err(e) => {
                assert_eq!(e.code, codes::RATE_LIMITED);
                limited += 1;
            }
        }
    }
    assert_eq!(ok, 5, "exactly the permit count passes");
    assert_eq!(limited, 15);
    assert_eq!(registry.bucket_count(), 1);
}

#[tokio::test]
async fn the_listing_shows_the_override() {
    let clock = manual_clock();
    let (handler, _) = FakeHandler::new();
    let (replacement, _) = FakeHandler::new();
    let mut b = FunctionRegistryBuilder::new();
    b.clock(clock);
    b.register_decl(
        "app1",
        spot_functions::FunctionDecl {
            key: ok_key(),
            verbs: None,
            guest: false,
            limit: Some(RateLimit {
                permits: 2,
                per: Duration::from_secs(1),
            }),
            arguments: None,
        },
        Arc::new(handler),
    );
    b.override_function(
        "app2",
        ok_key(),
        Arc::new(replacement),
        "site-specific behaviour",
    );
    let registry = b.build().unwrap();
    let list = registry.list();
    assert_eq!(list.len(), 1);
    let info = &list[0];
    assert_eq!(info.key, ok_key());
    assert_eq!(info.app, "app1");
    assert_eq!(info.limit.map(|l| l.permits), Some(2));
    assert_eq!(
        info.overridden_by.as_ref().map(|(a, _)| a.as_str()),
        Some("app2")
    );
    assert!(
        info.overridden_by
            .as_ref()
            .is_some_and(|(_, r)| r == "site-specific behaviour")
    );
}

#[tokio::test]
async fn dispatch_runs_the_override_handler_not_the_declaring_one() {
    // The override table decides which handler runs: the replacement answers, the original
    // app's handler never sees the call.
    let clock = manual_clock();
    let (original, original_log) = FakeHandler::new();
    let (replacement, replacement_log) = FakeHandler::new();
    let mut b = FunctionRegistryBuilder::new();
    b.clock(clock);
    b.register_decl("app1", plain_decl(), Arc::new(original));
    b.override_function(
        "app2",
        ok_key(),
        Arc::new(replacement),
        "site-specific behaviour",
    );
    let registry = b.build().unwrap();
    let reply = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap();
    assert_eq!(reply, serde_json::json!({ "ok": true }));
    assert_eq!(replacement_log.lock().unwrap().len(), 1);
    assert!(original_log.lock().unwrap().is_empty());
}

/// A plain, non-guest, limitless declaration of the suite's canonical function.
fn plain_decl() -> spot_functions::FunctionDecl {
    spot_functions::FunctionDecl {
        key: ok_key(),
        verbs: None,
        guest: false,
        limit: None,
        arguments: None,
    }
}
