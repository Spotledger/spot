//! The ownership snapshot on `/api/health/ready` (WP-PRD-01.T44). No database: a fake trail
//! source and a manual clock, as in `partition_snapshot.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use spot_cli::config::Site;
use spot_cli::services::partition::{
    Clock, MAX_STALENESS, SnapshotReady, SnapshotRegistry, TrailSource,
};
use spot_http::StaticSites;
use spot_http::health::ReadinessCheck;
use spot_http::{ServerBuilder, ServerConfig};
use spot_partition::{AssignmentRequest, PartitionChange, PartitionError, ScopeKey};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;
use tower::ServiceExt;

const SECRET: &str = "SECRET-STORE-ERR scope=acme/branch-7";
const NODE: &str = "node-secret-a";

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

struct FakeTrail {
    changes: Vec<PartitionChange>,
    failing: AtomicBool,
    /// While true, `try_history` waits (the store "blocks").
    gate: (Mutex<bool>, Condvar),
    entered: AtomicBool,
}

impl TrailSource for FakeTrail {
    fn try_history(&self) -> Result<Vec<PartitionChange>, PartitionError> {
        self.entered.store(true, Ordering::SeqCst);
        let (lock, cv) = &self.gate;
        let mut blocked = lock.lock().unwrap();
        while *blocked {
            blocked = cv.wait(blocked).unwrap();
        }
        drop(blocked);
        if self.failing.load(Ordering::SeqCst) {
            return Err(PartitionError::Store {
                reason: SECRET.to_owned(),
            });
        }
        Ok(self.changes.clone())
    }
    fn assign(&self, _: AssignmentRequest) -> Result<u64, PartitionError> {
        Err(PartitionError::Store {
            reason: "not in this fake".to_owned(),
        })
    }
    fn history(&self) -> Vec<PartitionChange> {
        self.changes.clone()
    }
}

struct World {
    trail: Arc<FakeTrail>,
    now: Arc<Mutex<OffsetDateTime>>,
    registry: Arc<SnapshotRegistry>,
}

fn world() -> World {
    let trail = Arc::new(FakeTrail {
        changes: vec![PartitionChange {
            scope: ScopeKey::new("branch", "acme/branch-7").unwrap(),
            node: NodeId::new(NODE).unwrap(),
            version: 1,
            effective_from: at(0),
            effective_to: None,
            actor: "admin".to_owned(),
            reason: "test".to_owned(),
            at: at(0),
        }],
        failing: AtomicBool::new(false),
        gate: (Mutex::new(false), Condvar::new()),
        entered: AtomicBool::new(false),
    });
    let now = Arc::new(Mutex::new(at(1_000)));
    let clock: Clock = {
        let now = Arc::clone(&now);
        Arc::new(move || *now.lock().unwrap())
    };
    let registry = SnapshotRegistry::start_with_clock(
        trail.clone(),
        at(1_000),
        Duration::from_secs(3600),
        clock,
    )
    .unwrap();
    World {
        trail,
        now,
        registry,
    }
}

fn advance(w: &World, seconds: i64) {
    *w.now.lock().unwrap() += time::Duration::seconds(seconds);
}

async fn body(check: SnapshotReady) -> (StatusCode, String) {
    let router = ServerBuilder::new(ServerConfig::default(), StaticSites::new())
        .readiness_check(check)
        .build();
    let response = router
        .oneshot(
            Request::get("/api/health/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn fresh_snapshot_is_ok_and_ready() {
    let w = world();
    let check = SnapshotReady::new("acme", Arc::clone(&w.registry));
    assert_eq!(check.name(), "partition:acme");
    let result = check.check().await;
    assert!(result.ok);
    assert!(result.detail.contains("age 0s"), "{}", result.detail);
    assert!(result.detail.contains("failures 0"), "{}", result.detail);
    assert!(!result.detail.starts_with("degraded:"));

    let (status, json) = body(check).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["status"], "ready");
    assert_eq!(v["checks"][0]["name"], "partition:acme");
}

#[tokio::test]
async fn stale_snapshot_is_degraded_never_fails_readiness_and_never_leaks() {
    let w = world();
    w.trail.failing.store(true, Ordering::SeqCst);
    assert!(w.registry.refresh().is_err());
    advance(&w, i64::try_from(MAX_STALENESS.as_secs()).unwrap() + 1);
    assert!(w.registry.refresh().is_err());

    let check = SnapshotReady::new("acme", Arc::clone(&w.registry));
    let result = check.check().await;
    assert!(result.ok, "reads are still served");
    assert!(result.detail.starts_with("degraded:"), "{}", result.detail);
    assert!(
        result
            .detail
            .contains("ownership snapshot stale: writes to partitioned scopes are refused")
    );

    let (status, json) = body(check).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.contains("degraded:"), "{json}");
    for secret in [SECRET, "SECRET-STORE-ERR", "acme/branch-7", NODE] {
        assert!(!json.contains(secret), "leaked {secret}: {json}");
    }
}

#[tokio::test]
async fn recovery_is_seen_on_the_next_call_with_the_same_registry() {
    let w = world();
    let before = Arc::as_ptr(&w.registry);
    w.trail.failing.store(true, Ordering::SeqCst);
    advance(&w, i64::try_from(MAX_STALENESS.as_secs()).unwrap() + 1);
    let check = SnapshotReady::new("acme", Arc::clone(&w.registry));
    assert!(check.check().await.detail.starts_with("degraded:"));

    w.trail.failing.store(false, Ordering::SeqCst);
    w.registry.refresh().unwrap();
    let result = check.check().await;
    assert!(result.ok);
    assert!(!result.detail.starts_with("degraded:"), "{}", result.detail);
    assert_eq!(Arc::as_ptr(&w.registry), before);
}

#[test]
fn a_site_without_partition_has_no_check() {
    let host = spot_cli::services::partition::Host::new();
    let site = Site::named("single", "db", "und", Vec::new());
    assert!(host.readiness(&site).is_none());
    assert!(host.registry("single").is_none());
}

#[tokio::test]
async fn health_does_not_wait_on_a_blocked_store() {
    let w = world();
    *w.trail.gate.0.lock().unwrap() = true;
    let registry = Arc::clone(&w.registry);
    let refresher = std::thread::spawn(move || {
        let _ = registry.refresh();
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !w.trail.entered.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "refresh never reached the store");
        std::thread::yield_now();
    }
    let started = Instant::now();
    let result = SnapshotReady::new("acme", Arc::clone(&w.registry))
        .check()
        .await;
    assert!(result.ok);
    assert!(started.elapsed() < Duration::from_millis(500));

    *w.trail.gate.0.lock().unwrap() = false;
    w.trail.gate.1.notify_all();
    refresher.join().unwrap();
}
