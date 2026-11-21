//! The cluster-wide rate path (WP-AI-05.T1): AI request tokens come from the coordinator, so a
//! fleet never admits more than the rate however many nodes race.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_ai_isolation::admission::{AiAdmission, AiRequest};
use spot_ai_isolation::config::{AiLimits, AiPoolSettings};
use spot_ai_isolation::error::RATE_LIMITED;
use spot_ai_isolation::identity::{AgentId, TenantId};
use spot_ai_isolation::load::StaticLoad;
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId, RateLimit};
use spot_core::SystemClock;

#[tokio::test]
async fn the_cluster_rate_limit_is_taken_from_the_coordinator() {
    let presence = Arc::new(InMemoryPresence::new());
    let node = NodeId::new("node-1").expect("a valid node id");
    let coordinator = InProcessCoordinator::start(presence, node, Arc::new(SystemClock::new()))
        .await
        .expect("the node starts alone");

    let limits = AiLimits {
        per_agent_rate: RateLimit {
            capacity: 1,
            per: Duration::from_secs(60),
        },
        ..AiLimits::default()
    };
    let gate = Arc::new(
        AiAdmission::new(
            limits,
            AiPoolSettings::default(),
            Arc::new(StaticLoad::calm()),
        )
        .expect("a valid configuration"),
    );
    let request = AiRequest::new(
        AgentId::new("a").expect("a valid agent"),
        TenantId::new("t").expect("a valid tenant"),
    );

    let first = gate
        .admit_cluster(coordinator.as_ref(), &request, Duration::ZERO)
        .await
        .expect("the first token is granted");
    drop(first);

    let error = gate
        .admit_cluster(coordinator.as_ref(), &request, Duration::ZERO)
        .await
        .err()
        .expect("the second call is rate limited");
    assert_eq!(error.code, RATE_LIMITED);
    assert!(error.retry_after.is_some());
    assert_eq!(gate.in_flight(), 0);
}
