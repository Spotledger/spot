//! The metered coordinator records lease events and the held gauge. One test: the metrics
//! recorder is process-global.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::metered_coordinator::MeteredCoordinator;
use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::ManualClock;
use time::OffsetDateTime;

fn value(key: &str) -> f64 {
    spot_telemetry::render_prometheus()
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

#[tokio::test]
async fn lease_lifecycle_is_counted() {
    spot_telemetry::metrics::install().unwrap();
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let inner: Arc<dyn Coordinator> = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        clock,
    )
    .await
    .unwrap();
    let coordinator = MeteredCoordinator::new(inner);

    let event = |e: &str| format!(r#"spot_lease_events_total{{lease="scheduler",event="{e}"}}"#);
    let held = r#"spot_lease_held{lease="scheduler"}"#;
    let before: Vec<f64> = ["acquired", "renewed", "released", "lost", "fence_refused"]
        .iter()
        .map(|e| value(&event(e)))
        .collect();
    let ttl = Duration::from_secs(10);
    let name = "scheduler:acme";

    let lease = coordinator.acquire(name, "node-1", ttl).await.unwrap();
    assert_eq!(value(held), 1.0);
    // A second acquire while held is refused and counts nothing.
    assert!(coordinator.acquire(name, "node-2", ttl).await.is_err());
    let lease = coordinator.renew(&lease, ttl).await.unwrap();
    coordinator.check_fence(name, lease.token).await.unwrap();
    // A stale token is refused and the fence refusal is counted.
    let stale = spot_coordinator::FencingToken(lease.token.0 + 100);
    assert!(coordinator.check_fence(name, stale).await.is_err());
    assert_eq!(value(held), 0.0);
    coordinator.release(&lease).await.unwrap();
    assert_eq!(value(held), 0.0);

    let after: Vec<f64> = ["acquired", "renewed", "released", "lost", "fence_refused"]
        .iter()
        .map(|e| value(&event(e)))
        .collect();
    let delta: Vec<f64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
    assert_eq!(delta, vec![1.0, 1.0, 1.0, 1.0, 1.0]);
    assert!(
        !spot_telemetry::render_prometheus()
            .lines()
            .any(|l| l.contains("spot_lease") && l.contains("acme"))
    );
}
