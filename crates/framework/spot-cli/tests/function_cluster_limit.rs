//! A function's declared limit holds across the nodes of a cluster (WP-PLT-17.T8): three
//! registries (three nodes) over one coordinator admit what one node would, a coordinator that
//! cannot answer never opens the gate, the coordinator keys name no user, the single-node
//! profile builds no coordinator gate and a registry rebuild keeps the gate.
//!
//! The coordinator is the in-process one (the logic is the same as the database one's: the
//! `Coordinator` contract tests hold both to it). A PostgreSQL run of the same three-node case
//! belongs in the bundle's `SPOT_TEST_PG_URL` pass once the function service installs the gate.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_cli::services::function_gate::{
    CoordinatorGate, GateInstaller, MeteredGate, coordinator_key,
};
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, Error, FencingToken, InMemoryPresence, InProcessCoordinator,
    Lease, NodeId, RateLimit as CoordLimit, Result as CoordResult,
};
use spot_core::{Clock, ManualClock};
use spot_functions::{
    Caller, Context, FunctionDecl, FunctionHandler, FunctionKey, FunctionRegistry,
    FunctionRegistryBuilder, GateKey, HandlerFuture, LocalGate, RateGate, RateLimit, codes,
};
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio::task::JoinSet;

const SITE: &str = "acme";

struct Noop;

impl FunctionHandler for Noop {
    fn call<'a>(&'a self, _ctx: &'a Context, _args: Json) -> HandlerFuture<'a> {
        Box::pin(async { Ok(json!({"ok": true})) })
    }
}

/// A coordinator that records the keys of every `take`, optionally fails every `take`, and
/// otherwise delegates to a real one.
struct Spy {
    inner: Arc<dyn Coordinator>,
    keys: Mutex<Vec<String>>,
    fail: bool,
}

impl Spy {
    fn seen(&self) -> Vec<String> {
        self.keys.lock().unwrap().clone()
    }
}

impl Coordinator for Spy {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, CoordResult<Lease>> {
        self.inner.acquire(name, holder, ttl)
    }
    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, CoordResult<Lease>> {
        self.inner.renew(lease, ttl)
    }
    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, CoordResult<()>> {
        self.inner.release(lease)
    }
    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<Option<Lease>>> {
        self.inner.current(name)
    }
    fn check_fence<'a>(
        &'a self,
        name: &'a str,
        token: FencingToken,
    ) -> BoxFuture<'a, CoordResult<()>> {
        self.inner.check_fence(name, token)
    }
    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.next(name, by)
    }
    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.peek(name)
    }
    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.set_at_least(name, value)
    }
    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: CoordLimit,
        cost: u64,
    ) -> BoxFuture<'a, CoordResult<Admit>> {
        self.keys.lock().unwrap().push(key.to_owned());
        if self.fail {
            return Box::pin(async {
                Err(Error::new(
                    "coordinator.unavailable",
                    "the coordinator is down",
                ))
            });
        }
        self.inner.take(key, limit, cost)
    }
    fn waker(&self) -> Arc<Notify> {
        self.inner.waker()
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH))
}

async fn coordinator(clock: &Arc<ManualClock>, fail: bool) -> Arc<Spy> {
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let inner: Arc<dyn Coordinator> = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        clock_dyn,
    )
    .await
    .unwrap();
    Arc::new(Spy {
        inner,
        keys: Mutex::new(Vec::new()),
        fail,
    })
}

fn key() -> FunctionKey {
    FunctionKey::from_parts("app1", "mod1", "fn1")
}

/// One node's registry: `app1/mod1/fn1` with the limit `permits` per minute.
fn registry(clock: &Arc<ManualClock>, permits: u32) -> FunctionRegistry {
    let mut builder = FunctionRegistryBuilder::new();
    builder.clock(clock.clone());
    builder.register_decl(
        "app1",
        FunctionDecl {
            key: key(),
            verbs: None,
            guest: false,
            limit: Some(RateLimit {
                permits,
                per: Duration::from_secs(60),
            }),
            arguments: None,
        },
        Arc::new(Noop),
    );
    builder.build().unwrap()
}

/// One node: its installer and the registry the installer put the gate on.
fn node(
    clock: &Arc<ManualClock>,
    profile: &str,
    coordinator: &Arc<Spy>,
    permits: u32,
) -> (Arc<GateInstaller>, Arc<FunctionRegistry>) {
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let shared: Arc<dyn Coordinator> = coordinator.clone();
    let installer = Arc::new(GateInstaller::new(profile, SITE, clock_dyn, Some(shared)));
    let registry = Arc::new(installer.apply(registry(clock, permits)));
    (installer, registry)
}

fn as_user(id: &str) -> Context {
    Context::new(SITE, Caller::User(id.to_owned()))
}

/// `calls` concurrent calls of the function by `user` on each registry; returns the admitted
/// count and the `retry_after` details of every refusal.
async fn storm(
    registries: &[Arc<FunctionRegistry>],
    user: &str,
    calls: usize,
) -> (usize, Vec<Json>) {
    let mut set = JoinSet::new();
    for registry in registries {
        for _ in 0..calls {
            let registry = Arc::clone(registry);
            let ctx = as_user(user);
            set.spawn(async move { registry.dispatch(&ctx, &key(), json!({})).await });
        }
    }
    let mut admitted = 0;
    let mut refused = Vec::new();
    while let Some(done) = set.join_next().await {
        match done.unwrap() {
            Ok(_) => admitted += 1,
            Err(error) => {
                assert_eq!(error.code, codes::RATE_LIMITED);
                assert!(error.retryable());
                refused.push(
                    error
                        .details
                        .get("retry_after")
                        .cloned()
                        .unwrap_or(Json::Null),
                );
            }
        }
    }
    (admitted, refused)
}

#[tokio::test]
async fn three_nodes_admit_exactly_the_declared_limit() {
    let clock = clock();
    let coordinator = coordinator(&clock, false).await;
    let nodes: Vec<_> = (0..3)
        .map(|_| node(&clock, "cluster", &coordinator, 10))
        .collect();
    let registries: Vec<_> = nodes.iter().map(|(_, r)| Arc::clone(r)).collect();
    let (admitted, refused) = storm(&registries, "u1", 30).await;
    assert_eq!(admitted, 10, "10 per minute for the site, not per node");
    assert_eq!(refused.len(), 80);
    let first = refused.first().cloned().unwrap();
    assert!(
        first.as_u64().is_some_and(|ms| ms > 0),
        "retry_after is whole milliseconds"
    );
    assert!(
        refused.iter().all(|r| *r == first),
        "every refusal has the same shape"
    );
    for (installer, _) in &nodes {
        assert!(installer.is_shared());
        assert_eq!(installer.fallbacks(), 0);
    }
}

#[tokio::test]
async fn the_limit_refills_after_the_window() {
    let clock = clock();
    let coordinator = coordinator(&clock, false).await;
    let registries: Vec<_> = (0..3)
        .map(|_| node(&clock, "cluster", &coordinator, 10).1)
        .collect();
    let (admitted, _) = storm(&registries, "u1", 10).await;
    assert_eq!(admitted, 10);
    let (admitted, _) = storm(&registries, "u1", 1).await;
    assert_eq!(admitted, 0, "spent");
    clock.advance(Duration::from_secs(60));
    let (admitted, refused) = storm(&registries, "u1", 10).await;
    assert_eq!(admitted, 10, "a full window later the allowance is back");
    assert_eq!(refused.len(), 20);
}

#[tokio::test]
async fn callers_have_their_own_bucket_and_one_caller_shares_it_over_the_nodes() {
    let clock = clock();
    let coordinator = coordinator(&clock, false).await;
    let registries: Vec<_> = (0..3)
        .map(|_| node(&clock, "cluster", &coordinator, 4).1)
        .collect();
    // u1 spends its 4 across three nodes, one call at a time round robin.
    let mut admitted = 0;
    for i in 0..9 {
        let registry = &registries[i % 3];
        if registry
            .dispatch(&as_user("u1"), &key(), json!({}))
            .await
            .is_ok()
        {
            admitted += 1;
        }
    }
    assert_eq!(admitted, 4);
    // u2 is untouched by u1's spending.
    let (admitted, _) = storm(&registries, "u2", 5).await;
    assert_eq!(admitted, 4);
}

#[tokio::test]
async fn a_coordinator_that_errors_never_opens_the_gate() {
    let clock = clock();
    let coordinator = coordinator(&clock, true).await;
    let nodes: Vec<_> = (0..3)
        .map(|_| node(&clock, "cluster", &coordinator, 5))
        .collect();
    let registries: Vec<_> = nodes.iter().map(|(_, r)| Arc::clone(r)).collect();
    let (admitted, refused) = storm(&registries, "u1", 20).await;
    assert_eq!(admitted, 15, "at most `permits` per node: 3 x 5");
    assert_eq!(refused.len(), 45);
    for (installer, _) in &nodes {
        assert_eq!(
            installer.fallbacks(),
            20,
            "every call fell back and was counted"
        );
    }
}

#[tokio::test]
async fn the_coordinator_key_names_no_user() {
    let clock = clock();
    let coordinator = coordinator(&clock, false).await;
    let (_, registry) = node(&clock, "cluster", &coordinator, 10);
    for user in ["alice-7f3a-secret-id", "bob-1c2d"] {
        registry
            .dispatch(&as_user(user), &key(), json!({}))
            .await
            .unwrap();
    }
    registry
        .dispatch(&Context::new(SITE, Caller::Anonymous), &key(), json!({}))
        .await
        .ok();
    let keys = coordinator.seen();
    assert!(keys.len() >= 2);
    for k in &keys {
        assert!(k.starts_with("function:acme:app1/mod1/fn1:"), "{k}");
        assert!(
            !k.contains("alice") && !k.contains("bob") && !k.contains("secret"),
            "{k}"
        );
        let caller = k.rsplit(':').next().unwrap();
        assert_eq!(caller.len(), 32, "a fixed-length hash: {k}");
        assert!(caller.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    assert_ne!(keys[0], keys[1], "two users, two buckets");
}

#[test]
fn a_long_function_address_still_fits_a_coordinator_name() {
    let long = format!("{0}/{0}/{0}", "a".repeat(63));
    let gate_key = GateKey {
        function: long.clone(),
        caller: "user:u1".to_owned(),
    };
    let name = coordinator_key(&"s".repeat(63), &gate_key);
    assert!(
        name.len() <= spot_coordinator::types::MAX_NAME_BYTES,
        "{}",
        name.len()
    );
    assert!(!name.contains(&long));
    // The key is stable: the same bucket always gets the same name.
    assert_eq!(name, coordinator_key(&"s".repeat(63), &gate_key));
}

#[tokio::test]
async fn the_single_node_profile_builds_no_coordinator_gate() {
    let clock = clock();
    let coordinator = coordinator(&clock, false).await;
    let (installer, registry) = node(&clock, "single", &coordinator, 2);
    assert!(!installer.is_shared());
    assert_eq!(installer.gates_built(), 0);
    let (admitted, refused) = storm(&[Arc::clone(&registry)], "u1", 5).await;
    assert_eq!(admitted, 2, "the node's own bucket still limits");
    assert_eq!(refused.len(), 3);
    assert!(
        coordinator.seen().is_empty(),
        "the coordinator was never asked"
    );
    assert_eq!(registry.bucket_count(), 1);
}

#[tokio::test]
async fn a_registry_rebuild_keeps_the_gate() {
    let clock = clock();
    let coordinator = coordinator(&clock, false).await;
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let shared: Arc<dyn Coordinator> = coordinator.clone();
    let installer = GateInstaller::new("cluster", SITE, clock_dyn, Some(shared));
    // The first build and the rebuild after an app swap: the service applies the installer to
    // every registry it builds.
    let first = Arc::new(installer.apply(registry(&clock, 3)));
    let rebuilt = Arc::new(installer.apply(registry(&clock, 3)));
    assert_eq!(installer.gates_built(), 2);
    let (admitted, _) = storm(&[first, Arc::clone(&rebuilt)], "u1", 3).await;
    assert_eq!(
        admitted, 3,
        "one limit for both builds: the rebuilt registry shares the buckets"
    );
    assert!(!coordinator.seen().is_empty());
}

#[tokio::test]
async fn eviction_and_bucket_count_keep_working_on_the_fallback() {
    let clock = clock();
    let coordinator = coordinator(&clock, true).await;
    let (_, registry) = node(&clock, "cluster", &coordinator, 5);
    registry
        .dispatch(&as_user("u1"), &key(), json!({}))
        .await
        .unwrap();
    assert_eq!(registry.bucket_count(), 1);
    clock.advance(Duration::from_secs(600));
    assert_eq!(registry.evict_idle_buckets(Duration::from_secs(60)), 1);
    assert_eq!(registry.bucket_count(), 0);
}

#[tokio::test]
async fn the_gates_compose_the_way_the_installer_builds_them() {
    // CoordinatorGate and MeteredGate are public so a host can build its own stack.
    let clock = clock();
    let coordinator = coordinator(&clock, false).await;
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let local = Arc::new(LocalGate::new(clock_dyn.clone()));
    let shared: Arc<dyn Coordinator> = coordinator.clone();
    let gate = MeteredGate::new(
        Arc::new(CoordinatorGate::new(shared, SITE, local, clock_dyn.clone())),
        clock_dyn,
    );
    let gate_key = GateKey {
        function: "app1/mod1/fn1".to_owned(),
        caller: "user:u1".to_owned(),
    };
    assert!(
        gate.admit(&gate_key, 1, Duration::from_secs(60))
            .await
            .is_ok()
    );
    assert!(
        gate.admit(&gate_key, 1, Duration::from_secs(60))
            .await
            .is_err()
    );
}
