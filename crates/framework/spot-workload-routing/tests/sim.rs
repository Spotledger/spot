//! The acceptance simulation (WP-PLT-20.T1): heavy report load must leave interactive latency
//! within budget, and without routing it must not.
//!
//! The scenario is the brief's: three nodes — two interactive, one report — a heavy report burst
//! arriving at once, and steady interactive traffic arriving throughout. The simulation is
//! deterministic and runs on virtual time ([`spot_workload_routing::sim`]), so the assertions are
//! exact and repeatable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use spot_workload_routing::class::WorkloadClass;
use spot_workload_routing::node::{Capacity, NodeProfile, NodeRole};
use spot_workload_routing::policy::RoutingPolicy;
use spot_workload_routing::sim::{SimConfig, SimRequest, p95, simulate};

/// The interactive latency budget, 200 ms.
const BUDGET_MS: u64 = 200;
/// How many interactive requests arrive over the run.
const INTERACTIVE_REQUESTS: u64 = 300;
/// How often one arrives, in virtual milliseconds.
const INTERACTIVE_EVERY_MS: u64 = 10;
/// How long one interactive request needs on a worker.
const INTERACTIVE_SERVICE_MS: u64 = 4;
/// How many reports arrive at once in the burst.
const REPORT_BURST: u64 = 120;
/// How long one report needs on a worker.
const REPORT_SERVICE_MS: u64 = 250;

/// The brief's topology: two interactive nodes and one dedicated report node.
fn topology() -> Vec<NodeProfile> {
    vec![
        NodeProfile::from_topology(
            "interactive-1",
            vec![NodeRole::Interactive],
            Vec::new(),
            Capacity::new(8, 1_000),
        ),
        NodeProfile::from_topology(
            "interactive-2",
            vec![NodeRole::Interactive],
            Vec::new(),
            Capacity::new(8, 1_000),
        ),
        NodeProfile::from_topology(
            "report-1",
            vec![NodeRole::Report],
            Vec::new(),
            Capacity::new(2, 1_000),
        ),
    ]
}

/// Queue affinity: interactive work only on the interactive nodes, report work only on the report
/// node. This is what keeps a click out of a report's queue. The standard policy already encodes
/// this shape; the test only pins the interactive budget.
fn policy() -> RoutingPolicy {
    RoutingPolicy {
        interactive_budget_ms: BUDGET_MS,
        ..RoutingPolicy::standard()
    }
}

/// The traffic: a report burst at t=0, then steady interactive arrivals.
fn requests() -> Vec<SimRequest> {
    let mut requests: Vec<SimRequest> = (0..REPORT_BURST)
        .map(|id| SimRequest {
            id,
            class: WorkloadClass::Report,
            arrival_ms: 0,
            service_ms: REPORT_SERVICE_MS,
        })
        .collect();
    for i in 0..INTERACTIVE_REQUESTS {
        requests.push(SimRequest {
            id: REPORT_BURST + i,
            class: WorkloadClass::Interactive,
            arrival_ms: i * INTERACTIVE_EVERY_MS,
            service_ms: INTERACTIVE_SERVICE_MS,
        });
    }
    requests
}

fn config(routing: bool) -> SimConfig {
    SimConfig {
        nodes: topology(),
        requests: requests(),
        policy: policy(),
        routing,
    }
}

#[test]
fn interactive_p95_stays_within_budget_with_routing_on() {
    let policy = policy();
    let outcome = simulate(&config(true));
    let interactive = outcome.latencies_for(WorkloadClass::Interactive);
    assert_eq!(
        interactive.len() as u64,
        INTERACTIVE_REQUESTS,
        "every interactive request completes"
    );
    let measured = p95(&interactive);
    assert!(
        measured <= policy.interactive_budget(),
        "interactive p95 {measured:?} must stay within the {} ms budget",
        policy.interactive_budget_ms
    );
    // The heavy lane still drains: routing moves work, it does not drop it.
    assert_eq!(
        outcome.completed_for(WorkloadClass::Report) as u64,
        REPORT_BURST
    );
}

#[test]
fn interactive_p95_exceeds_budget_with_routing_off() {
    let policy = policy();
    let outcome = simulate(&config(false).routing_off());
    let interactive = outcome.latencies_for(WorkloadClass::Interactive);
    assert_eq!(interactive.len() as u64, INTERACTIVE_REQUESTS);
    let measured = p95(&interactive);
    assert!(
        measured > policy.interactive_budget(),
        "without routing, the report burst must push interactive p95 ({measured:?}) past the \
         {} ms budget; the control only proves the scenario is heavy enough",
        policy.interactive_budget_ms
    );
    assert!(
        measured > Duration::from_secs(1),
        "the control must be decisively over budget, not marginally ({measured:?})"
    );
    assert_eq!(
        outcome.completed_for(WorkloadClass::Report) as u64,
        REPORT_BURST
    );
}

#[test]
fn routing_is_what_keeps_the_click_out_of_the_report_queue() {
    let routed = simulate(&config(true));
    let unrouted = simulate(&config(false).routing_off());
    let routed_p95 = p95(&routed.latencies_for(WorkloadClass::Interactive));
    let unrouted_p95 = p95(&unrouted.latencies_for(WorkloadClass::Interactive));
    assert!(
        routed_p95 < unrouted_p95,
        "routing on ({routed_p95:?}) must beat routing off ({unrouted_p95:?})"
    );
    assert_eq!(
        routed_p95,
        Duration::from_millis(INTERACTIVE_SERVICE_MS),
        "with a dedicated pool the interactive p95 is a single service time"
    );
}
