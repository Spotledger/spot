//! The ownership snapshot: a maximum staleness after which it fails closed, and a truncation that
//! never drops an assignment still in force (WP-PRD-01.T24). No database: a fake trail source.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_cli::services::Level;
use spot_cli::services::partition::{
    Clock, MAX_CHANGES, MAX_STALENESS, SnapshotRegistry, TrailSource,
};
use spot_partition::{
    AssignmentRequest, PartitionChange, PartitionError, PartitionRegistry, ScopeKey,
};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

fn scope(key: &str) -> ScopeKey {
    ScopeKey::new("branch", key).unwrap()
}

fn change(key: &str, node: &str, version: u64, from: i64, to: Option<i64>) -> PartitionChange {
    PartitionChange {
        scope: scope(key),
        node: NodeId::new(node).unwrap(),
        version,
        effective_from: at(from),
        effective_to: to.map(at),
        actor: "admin".to_owned(),
        reason: "test".to_owned(),
        at: at(from),
    }
}

#[derive(Default)]
struct FakeTrail {
    changes: Mutex<Vec<PartitionChange>>,
    failing: AtomicBool,
}

impl TrailSource for FakeTrail {
    fn try_history(&self) -> Result<Vec<PartitionChange>, PartitionError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(PartitionError::Store {
                reason: "down".to_owned(),
            });
        }
        Ok(self.changes.lock().unwrap().clone())
    }
    fn assign(&self, _: AssignmentRequest) -> Result<u64, PartitionError> {
        Err(PartitionError::Store {
            reason: "not in this fake".to_owned(),
        })
    }
    fn history(&self) -> Vec<PartitionChange> {
        self.changes.lock().unwrap().clone()
    }
}

struct World {
    trail: Arc<FakeTrail>,
    now: Arc<Mutex<OffsetDateTime>>,
    registry: Arc<SnapshotRegistry>,
}

fn world(changes: Vec<PartitionChange>, start: i64) -> World {
    let trail = Arc::new(FakeTrail::default());
    *trail.changes.lock().unwrap() = changes;
    let now = Arc::new(Mutex::new(at(start)));
    let clock: Clock = {
        let now = Arc::clone(&now);
        Arc::new(move || *now.lock().unwrap())
    };
    let registry = SnapshotRegistry::start_with_clock(
        trail.clone(),
        at(start),
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

fn advance(world: &World, seconds: i64) {
    let mut now = world.now.lock().unwrap();
    *now += time::Duration::seconds(seconds);
}

fn owner(world: &World, key: &str, instant: i64) -> Option<String> {
    world
        .registry
        .map_at(at(instant))
        .owner(&scope(key))
        .map(|n| n.as_str().to_owned())
}

#[test]
fn a_stale_snapshot_fails_closed_and_a_good_refresh_restores_it() {
    let w = world(vec![change("W1", "node-a", 1, 0, None)], 1_000);
    assert_eq!(owner(&w, "W1", 1_000).as_deref(), Some("node-a"));
    assert!(!w.registry.is_stale());

    w.trail.failing.store(true, Ordering::SeqCst);
    advance(&w, 100);
    assert!(w.registry.refresh().is_err());
    assert_eq!(
        owner(&w, "W1", 1_100).as_deref(),
        Some("node-a"),
        "within MAX_STALENESS the last good snapshot is served"
    );
    let limit = i64::try_from(MAX_STALENESS.as_secs()).unwrap();
    advance(&w, limit - 100);
    assert_eq!(
        owner(&w, "W1", 1_000).as_deref(),
        Some("node-a"),
        "exactly at the limit is still fresh"
    );
    advance(&w, 1);
    assert_eq!(
        owner(&w, "W1", 1_000),
        None,
        "past it, every write is refused"
    );
    assert!(w.registry.is_stale());
    assert!(w.registry.refresh().is_err());
    assert!(w.registry.map_at(at(1_000)).entries().is_empty());
    assert_eq!(
        w.registry.stale_transitions(),
        1,
        "the stale line fires once per transition, not per write"
    );

    w.trail.failing.store(false, Ordering::SeqCst);
    w.registry.refresh().unwrap();
    assert!(!w.registry.is_stale());
    assert_eq!(owner(&w, "W1", 1_000).as_deref(), Some("node-a"));

    // A second outage is a second transition.
    w.trail.failing.store(true, Ordering::SeqCst);
    advance(&w, limit + 1);
    assert!(w.registry.is_stale());
    assert_eq!(w.registry.stale_transitions(), 2);
}

#[test]
fn the_oldest_active_assignments_survive_truncation() {
    // 100 oldest: open-ended, never superseded. Then 4,900 closed assignments.
    let mut changes = Vec::new();
    for i in 0..100 {
        changes.push(change(&format!("A{i}"), "node-a", 1, 0, None));
    }
    for i in 0..4_900 {
        changes.push(change(&format!("C{i}"), "node-b", 1, 0, Some(10)));
    }
    let w = world(changes, 1_000);
    for i in 0..100 {
        assert_eq!(
            owner(&w, &format!("A{i}"), 1_000).as_deref(),
            Some("node-a"),
            "active assignment A{i} still resolves"
        );
    }
    assert_eq!(w.registry.changes(), MAX_CHANGES);
    assert_eq!(
        owner(&w, "C0", 5),
        None,
        "closed history was what got dropped"
    );
}

#[test]
fn active_assignments_are_all_kept_even_above_the_limit() {
    let changes = (0..5_000)
        .map(|i| change(&format!("K{i}"), "node-a", 1, 0, None))
        .collect();
    let w = world(changes, 1_000);
    assert_eq!(w.registry.changes(), 5_000);
    assert_eq!(owner(&w, "K0", 1_000).as_deref(), Some("node-a"));
    assert_eq!(owner(&w, "K4999", 1_000).as_deref(), Some("node-a"));
}

#[test]
fn a_superseded_version_goes_before_a_live_one() {
    // Key "S" has version 1 (closed, superseded) first, then 4,096 other keys that fill the
    // window, among them S version 2 (live) at the very end.
    let mut changes = vec![change("S", "node-a", 1, 0, Some(500))];
    for i in 0..4_095 {
        changes.push(change(&format!("L{i}"), "node-a", 1, 0, None));
    }
    changes.push(change("S", "node-b", 2, 500, None));
    assert_eq!(changes.len(), MAX_CHANGES + 1);
    let w = world(changes, 1_000);
    assert_eq!(w.registry.changes(), MAX_CHANGES);
    assert_eq!(owner(&w, "S", 1_000).as_deref(), Some("node-b"));
    assert_eq!(
        owner(&w, "S", 100),
        None,
        "the superseded version was dropped"
    );
    assert_eq!(owner(&w, "L0", 1_000).as_deref(), Some("node-a"));
}

#[test]
fn a_registry_needs_a_first_good_read() {
    let trail = Arc::new(FakeTrail::default());
    trail.failing.store(true, Ordering::SeqCst);
    let clock: Clock = Arc::new(|| at(0));
    let error = SnapshotRegistry::start_with_clock(trail, at(0), Duration::from_secs(3600), clock);
    assert!(error.is_err());
}

#[test]
fn health_reports_age_failures_and_staleness_without_leaking() {
    let w = world(vec![change("W1", "node-a", 1, 0, None)], 1_000);
    let fresh = w.registry.health();
    assert!(!fresh.degraded);
    assert_eq!(fresh.level, Level::Optional);
    assert!(fresh.detail.contains("age 0s"), "{}", fresh.detail);
    assert!(fresh.detail.contains("failures 0"), "{}", fresh.detail);

    w.trail.failing.store(true, Ordering::SeqCst);
    advance(&w, 60);
    assert!(w.registry.refresh().is_err());
    let aging = w.registry.health();
    assert!(!aging.degraded);
    assert!(aging.detail.contains("age 60s"), "{}", aging.detail);
    assert!(aging.detail.contains("failures 1"), "{}", aging.detail);

    let limit = i64::try_from(MAX_STALENESS.as_secs()).unwrap();
    advance(&w, limit);
    let stale = w.registry.health();
    assert!(stale.degraded);
    assert_eq!(stale.level, Level::Optional, "never required");
    assert!(
        stale
            .detail
            .contains("ownership snapshot stale: writes to partitioned scopes are refused"),
        "{}",
        stale.detail
    );
    assert!(stale.detail.contains(&format!("age {}s", limit + 60)));
    for leak in ["W1", "node-a", "down", "branch", "admin"] {
        assert!(!stale.detail.contains(leak), "detail leaks {leak}");
    }

    w.trail.failing.store(false, Ordering::SeqCst);
    w.registry.refresh().unwrap();
    let back = w.registry.health();
    assert!(!back.degraded, "no latch");
    assert!(back.detail.contains("age 0s"), "{}", back.detail);
}

struct LeakyTrail;

impl TrailSource for LeakyTrail {
    fn try_history(&self) -> Result<Vec<PartitionChange>, PartitionError> {
        Err(PartitionError::Store {
            reason: "SECRET-STORE-TEXT raw-statement-text".to_owned(),
        })
    }
    fn assign(&self, _: AssignmentRequest) -> Result<u64, PartitionError> {
        Err(PartitionError::Store {
            reason: "x".to_owned(),
        })
    }
    fn history(&self) -> Vec<PartitionChange> {
        Vec::new()
    }
}

#[test]
fn a_store_error_text_never_reaches_the_health_detail() {
    // The first read must succeed to start; use a trail that fails afterwards.
    struct Flaky(FakeTrail, LeakyTrail);
    impl TrailSource for Flaky {
        fn try_history(&self) -> Result<Vec<PartitionChange>, PartitionError> {
            if self.0.failing.load(Ordering::SeqCst) {
                self.1.try_history()
            } else {
                self.0.try_history()
            }
        }
        fn assign(&self, r: AssignmentRequest) -> Result<u64, PartitionError> {
            self.0.assign(r)
        }
        fn history(&self) -> Vec<PartitionChange> {
            self.0.history()
        }
    }
    let flaky = Arc::new(Flaky(FakeTrail::default(), LeakyTrail));
    let now = Arc::new(Mutex::new(at(0)));
    let clock: Clock = {
        let now = Arc::clone(&now);
        Arc::new(move || *now.lock().unwrap())
    };
    let registry =
        SnapshotRegistry::start_with_clock(flaky.clone(), at(0), Duration::from_secs(3600), clock)
            .unwrap();
    flaky.0.failing.store(true, Ordering::SeqCst);
    *now.lock().unwrap() = at(i64::try_from(MAX_STALENESS.as_secs()).unwrap() + 5);
    assert!(registry.refresh().is_err());
    let health = registry.health();
    assert!(health.degraded);
    assert!(!health.detail.contains("SECRET-STORE-TEXT"));
    assert!(!health.detail.contains("raw-statement-text"));
}
