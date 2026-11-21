//! The contract suite against the in-process backend, and the single-node rules that only it has.

use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::contract::{Harness, run_all};
use spot_coordinator::error::SECOND_NODE;
use spot_coordinator::{
    Coordinator, InMemoryPresence, InProcessCoordinator, NodeId, NodePresence, RateLimit,
};
use spot_core::ManualClock;
use time::OffsetDateTime;

fn node(id: &str) -> NodeId {
    match NodeId::new(id) {
        Ok(node) => node,
        Err(error) => panic!("node id {id}: {error}"),
    }
}

async fn start(
    presence: &InMemoryPresence,
    id: &str,
    clock: Arc<ManualClock>,
) -> spot_coordinator::Result<Arc<InProcessCoordinator>> {
    InProcessCoordinator::start(Arc::new(presence.clone()), node(id), clock).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_process_passes_the_contract_suite() {
    let report = run_all(|| async {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let coordinator = match start(&InMemoryPresence::new(), "node-1", Arc::clone(&clock)).await
        {
            Ok(coordinator) => coordinator as Arc<dyn Coordinator>,
            Err(error) => panic!("start: {error}"),
        };
        Harness { coordinator, clock }
    })
    .await;
    report.assert_all_passed();
    assert!(
        report.cases.len() >= 13,
        "only {} cases ran",
        report.cases.len()
    );
}

#[tokio::test]
async fn refuses_to_start_beside_a_second_node() {
    let presence = InMemoryPresence::new();
    presence.add_node(&node("node-9"));
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let error = match start(&presence, "node-1", clock).await {
        Ok(_) => panic!("started beside another node"),
        Err(error) => error,
    };
    assert_eq!(error.code, SECOND_NODE);
    assert!(
        error.message.contains("node-9"),
        "the other node is not named: {error}"
    );
    // The failed start withdrew its registration: node-9 is alone again.
    let others = presence.others(&node("node-9")).await;
    assert_eq!(others.map(|v| v.len()), Ok(0));
}

#[tokio::test]
async fn a_second_node_appearing_stops_every_call() {
    let presence = InMemoryPresence::new();
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let c = match start(&presence, "node-1", clock).await {
        Ok(c) => c,
        Err(error) => panic!("start: {error}"),
    };
    let ttl = Duration::from_secs(10);
    let lease = match c.acquire("job", "node-1", ttl).await {
        Ok(lease) => lease,
        Err(error) => panic!("acquire: {error}"),
    };
    assert!(c.check_presence().await.is_ok());
    presence.add_node(&node("node-2"));
    let error = c.check_presence().await.err();
    assert_eq!(error.map(|e| e.code), Some(SECOND_NODE));
    // Every later call fails, even after the other node left: no silent double grants.
    presence.remove_node(&node("node-2"));
    let limit = RateLimit {
        capacity: 1,
        per: Duration::from_secs(1),
    };
    assert_eq!(
        c.acquire("job2", "node-1", ttl).await.err().map(|e| e.code),
        Some(SECOND_NODE)
    );
    assert_eq!(
        c.renew(&lease, ttl).await.err().map(|e| e.code),
        Some(SECOND_NODE)
    );
    assert_eq!(
        c.release(&lease).await.err().map(|e| e.code),
        Some(SECOND_NODE)
    );
    assert_eq!(
        c.current("job").await.err().map(|e| e.code),
        Some(SECOND_NODE)
    );
    assert_eq!(
        c.next("n", 1).await.err().map(|e| e.code),
        Some(SECOND_NODE)
    );
    assert_eq!(
        c.take("k", limit, 1).await.err().map(|e| e.code),
        Some(SECOND_NODE)
    );
    assert_eq!(
        c.check_presence().await.err().map(|e| e.code),
        Some(SECOND_NODE)
    );
}

#[tokio::test]
async fn dropping_the_coordinator_withdraws_the_node() {
    let presence = InMemoryPresence::new();
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let first = match start(&presence, "node-1", Arc::clone(&clock)).await {
        Ok(c) => c,
        Err(error) => panic!("start: {error}"),
    };
    assert!(
        start(&presence, "node-2", Arc::clone(&clock))
            .await
            .is_err()
    );
    drop(first);
    assert!(start(&presence, "node-2", clock).await.is_ok());
}
