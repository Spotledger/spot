//! The rate gate seam: the registry asks the gate (function and caller bucket), a denying
//! gate surfaces as `function.rate_limited` with its `retry_after`, the local gate is the old
//! behaviour, and a slow gate for one function does not hold up another.
//!
//! A gate that panics is not caught by the registry (there is no `catch_unwind` around the
//! gate); that is deliberate and not tested here.
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use spot_functions::{
    FunctionDecl, FunctionKey, FunctionRegistry, FunctionRegistryBuilder, GateFuture, GateKey,
    LimitError, LocalGate, RateGate, RateLimit, codes,
};
use tokio::sync::Notify;

/// What the fake gate was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Asked {
    key: GateKey,
    permits: u32,
    per: Duration,
}

/// A gate that records every question and answers from a fixed verdict.
struct FakeGate {
    asked: Mutex<Vec<Asked>>,
    deny_after: Option<Duration>,
}

impl FakeGate {
    fn allowing() -> Arc<FakeGate> {
        Arc::new(FakeGate {
            asked: Mutex::new(Vec::new()),
            deny_after: None,
        })
    }

    fn denying(retry_after: Duration) -> Arc<FakeGate> {
        Arc::new(FakeGate {
            asked: Mutex::new(Vec::new()),
            deny_after: Some(retry_after),
        })
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }
}

impl RateGate for FakeGate {
    fn admit<'a>(&'a self, key: &'a GateKey, permits: u32, per: Duration) -> GateFuture<'a> {
        self.asked.lock().unwrap().push(Asked {
            key: key.clone(),
            permits,
            per,
        });
        let verdict = match self.deny_after {
            Some(retry_after) => Err(LimitError { retry_after }),
            None => Ok(()),
        };
        Box::pin(async move { verdict })
    }
}

/// A gate that holds the call of one function until released.
struct SlowGate {
    slow_function: String,
    release: Arc<Notify>,
}

impl RateGate for SlowGate {
    fn admit<'a>(&'a self, key: &'a GateKey, _permits: u32, _per: Duration) -> GateFuture<'a> {
        let hold = key.function == self.slow_function;
        let release = self.release.clone();
        Box::pin(async move {
            if hold {
                release.notified().await;
            }
            Ok(())
        })
    }
}

fn decl(name: &str, limit: Option<RateLimit>) -> FunctionDecl {
    FunctionDecl {
        key: key("app1", "mod1", name),
        verbs: None,
        guest: false,
        limit,
        arguments: None,
    }
}

fn registry_with(decls: Vec<FunctionDecl>) -> FunctionRegistry {
    let mut b = FunctionRegistryBuilder::new();
    b.clock(manual_clock());
    for d in decls {
        let (handler, _) = FakeHandler::new();
        b.register_decl("app1", d, Arc::new(handler));
    }
    b.build().unwrap()
}

fn per_minute(permits: u32) -> Option<RateLimit> {
    Some(RateLimit {
        permits,
        per: Duration::from_secs(60),
    })
}

#[tokio::test]
async fn the_registry_asks_the_gate_with_the_function_and_the_caller_bucket() {
    let gate = FakeGate::allowing();
    let registry = registry_with(vec![decl("fn1", per_minute(7))]).with_gate(gate.clone());
    registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap();
    assert_eq!(
        gate.asked(),
        vec![Asked {
            key: GateKey {
                function: "app1/mod1/fn1".to_owned(),
                caller: "user:u1".to_owned(),
            },
            permits: 7,
            per: Duration::from_secs(60),
        }]
    );
}

#[tokio::test]
async fn a_function_without_a_limit_never_asks_the_gate() {
    let gate = FakeGate::denying(Duration::from_secs(1));
    let registry = registry_with(vec![decl("fn1", None)]).with_gate(gate.clone());
    registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap();
    assert!(gate.asked().is_empty());
}

#[tokio::test]
async fn a_denying_gate_yields_rate_limited_with_its_retry_after() {
    let gate = FakeGate::denying(Duration::from_millis(4321));
    let registry = registry_with(vec![decl("fn1", per_minute(7))]).with_gate(gate);
    let err = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::RATE_LIMITED);
    assert!(err.retryable());
    assert_eq!(
        err.details.get("retry_after"),
        Some(&serde_json::json!(4321_u64))
    );
}

#[tokio::test]
async fn the_default_gate_is_the_local_bucket() {
    let clock = manual_clock();
    let mut b = FunctionRegistryBuilder::new();
    b.clock(clock.clone());
    let (handler, _) = FakeHandler::new();
    b.register_decl("app1", decl("fn1", per_minute(2)), Arc::new(handler));
    let registry = b.build().unwrap();
    for _ in 0..2 {
        registry
            .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
            .await
            .unwrap();
    }
    let err = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::RATE_LIMITED);
    assert_eq!(registry.bucket_count(), 1);
    clock.advance(Duration::from_secs(120));
    assert_eq!(registry.evict_idle_buckets(Duration::from_secs(60)), 1);
    assert_eq!(registry.bucket_count(), 0);
}

#[tokio::test]
async fn a_local_gate_alone_matches_the_registry_default() {
    let clock = manual_clock();
    let gate = LocalGate::new(clock.clone());
    let key = GateKey {
        function: "app1/mod1/fn1".to_owned(),
        caller: "user:u1".to_owned(),
    };
    let per = Duration::from_secs(10);
    assert!(gate.admit(&key, 1, per).await.is_ok());
    let err = gate.admit(&key, 1, per).await.unwrap_err();
    assert_eq!(err.retry_after, Duration::from_secs(10));
    clock.advance(Duration::from_secs(10));
    assert!(gate.admit(&key, 1, per).await.is_ok());
}

#[tokio::test]
async fn a_gate_that_keeps_no_buckets_reports_none() {
    let gate = FakeGate::allowing();
    let registry = registry_with(vec![decl("fn1", per_minute(7))]).with_gate(gate);
    registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap();
    assert_eq!(registry.bucket_count(), 0);
    assert_eq!(registry.evict_idle_buckets(Duration::ZERO), 0);
}

#[tokio::test]
async fn a_slow_gate_for_one_function_does_not_block_another() {
    let release = Arc::new(Notify::new());
    let gate = Arc::new(SlowGate {
        slow_function: "app1/mod1/fn1".to_owned(),
        release: release.clone(),
    });
    let registry =
        registry_with(vec![decl("fn1", per_minute(5)), decl("fn2", per_minute(5))]).with_gate(gate);
    let fast_key: FunctionKey = key("app1", "mod1", "fn2");
    let slow_ctx = ctx_for(user("u1"), None);
    let fast_ctx = ctx_for(user("u1"), None);
    let slow_key = ok_key();
    let slow_call = registry.dispatch(&slow_ctx, &slow_key, json_args());
    let fast_call = async {
        let result = registry.dispatch(&fast_ctx, &fast_key, json_args()).await;
        // Only after the other function has been served does the slow gate let go; if the
        // fast call had to wait for the slow one this join would never finish.
        release.notify_one();
        result
    };
    let (slow, fast) = tokio::join!(slow_call, fast_call);
    assert!(slow.is_ok());
    assert!(fast.is_ok());
}
