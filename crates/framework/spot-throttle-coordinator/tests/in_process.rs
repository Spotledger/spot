//! The throttle cases against the in-process coordinator.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod suite;

use std::sync::Arc;

use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::ManualClock;
use suite::Env;
use time::OffsetDateTime;

async fn env() -> Option<Env> {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let coordinator = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    )
    .await
    .unwrap();
    // The in-process coordinator is one node by design; "two nodes on one store" is two
    // throttles over this one coordinator. The PostgreSQL file runs the true multi-node case.
    let node = coordinator as Arc<dyn Coordinator>;
    Some(Env {
        nodes: vec![Arc::clone(&node), node],
        clock: Some(clock),
        // The in-process coordinator has no database.
        db: None,
    })
}

suite::cases!(env);
