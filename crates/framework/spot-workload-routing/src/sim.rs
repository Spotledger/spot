//! A discrete-event load simulation over virtual time (WP-PLT-20.T1).
//!
//! The acceptance for WP-PLT-20 is a latency claim: *heavy report load on report nodes leaves
//! interactive latency within budget*. A claim like that needs a model that can be run and read,
//! so this module is a small, deterministic simulator:
//!
//! - Nodes are worker pools. Each pool has a fixed number of workers and one FIFO queue.
//! - A [`SimRequest`] arrives at a virtual time, belongs to a class, and needs a fixed service
//!   time on one worker.
//! - With `routing = true`, requests are placed in the pool for their class, derived from the
//!   policy's queue affinity ([`crate::policy::RoutingPolicy::node_serves`]): interactive traffic
//!   and report traffic never share a worker.
//! - With `routing = false` (the control), every node is one pool: all traffic shares one queue.
//!
//! Time is integer milliseconds and the event loop is a heap ordered by `(time, sequence)`, so a
//! run is reproducible and reads no clock. [`percentile`] is integer arithmetic (no floats), and
//! the p95 the tests assert on is the 95th percentile of interactive completion latency.

use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::time::Duration;

use crate::class::WorkloadClass;
use crate::node::NodeProfile;
use crate::policy::RoutingPolicy;

/// One simulated request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimRequest {
    /// A stable id, for reporting.
    pub id: u64,
    /// The class the request belongs to.
    pub class: WorkloadClass,
    /// When it arrives, in virtual milliseconds.
    pub arrival_ms: u64,
    /// How long one worker needs, in virtual milliseconds.
    pub service_ms: u64,
}

/// A simulation configuration.
#[derive(Clone, Debug)]
pub struct SimConfig {
    /// The topology.
    pub nodes: Vec<NodeProfile>,
    /// The requests.
    pub requests: Vec<SimRequest>,
    /// The policy the placement derives from.
    pub policy: RoutingPolicy,
    /// Whether class-aware routing is on. Off is the control: one shared pool.
    pub routing: bool,
}

impl SimConfig {
    /// The same configuration with routing off (the control run).
    #[must_use]
    pub fn routing_off(mut self) -> Self {
        self.routing = false;
        self
    }
}

/// One completion: when a request finished and how long it took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimCompletion {
    /// The request's id.
    pub request_id: u64,
    /// The request's class.
    pub class: WorkloadClass,
    /// How long the request waited, end to end.
    pub latency: Duration,
}

/// What a simulation produced.
#[derive(Clone, Debug, Default)]
pub struct SimOutcome {
    /// Every completion, in completion order.
    pub completions: Vec<SimCompletion>,
}

impl SimOutcome {
    /// The latencies of one class, in completion order.
    #[must_use]
    pub fn latencies_for(&self, class: WorkloadClass) -> Vec<Duration> {
        self.completions
            .iter()
            .filter(|c| c.class == class)
            .map(|c| c.latency)
            .collect()
    }

    /// How many requests of a class completed.
    #[must_use]
    pub fn completed_for(&self, class: WorkloadClass) -> usize {
        self.completions.iter().filter(|c| c.class == class).count()
    }
}

/// The `p`-th percentile (0..=100) of a set of durations, by nearest rank. Integer arithmetic
/// only (ADR-12): no floats anywhere.
#[must_use]
pub fn percentile(values: &[Duration], p: u32) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted: Vec<Duration> = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let rank = ((p as usize) * n).div_ceil(100);
    let index = rank.saturating_sub(1).min(n - 1);
    sorted[index]
}

/// The 95th percentile, by nearest rank.
#[must_use]
pub fn p95(values: &[Duration]) -> Duration {
    percentile(values, 95)
}

/// A worker pool: its workers, how many are free, and its FIFO queue.
struct Pool {
    free: u32,
    waiting: VecDeque<usize>,
}

/// What a heap entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EventKind {
    Arrival { index: usize },
    Complete { pool: usize, index: usize },
}

/// Run the simulation. Returns every completion; the caller asserts on latencies.
#[must_use]
pub fn simulate(config: &SimConfig) -> SimOutcome {
    // Build the pools and the class -> pool map.
    let mut pools: Vec<Pool> = Vec::new();
    let mut class_pool: BTreeMap<WorkloadClass, usize> = BTreeMap::new();
    if config.routing {
        for class in WorkloadClass::ALL {
            let workers: u32 = config
                .nodes
                .iter()
                .filter(|node| config.policy.node_serves(node, class))
                .map(|node| node.capacity.workers)
                .sum();
            if workers == 0 {
                continue;
            }
            class_pool.insert(class, pools.len());
            pools.push(Pool {
                free: workers,
                waiting: VecDeque::new(),
            });
        }
    }
    // Every class with no dedicated nodes shares one pool of all nodes.
    let shared_workers: u32 = config.nodes.iter().map(|node| node.capacity.workers).sum();
    let shared = pools.len();
    pools.push(Pool {
        free: shared_workers,
        waiting: VecDeque::new(),
    });

    let pool_for = |class: WorkloadClass| -> usize {
        if config.routing {
            class_pool.get(&class).copied().unwrap_or(shared)
        } else {
            0
        }
    };

    // Event heap: (time, sequence, kind). The sequence keeps ties deterministic.
    let mut heap: BinaryHeap<std::cmp::Reverse<(u64, u64, EventKind)>> = BinaryHeap::new();
    for (index, request) in config.requests.iter().enumerate() {
        heap.push(std::cmp::Reverse((
            request.arrival_ms,
            index as u64,
            EventKind::Arrival { index },
        )));
    }

    let mut outcome = SimOutcome::default();
    while let Some(std::cmp::Reverse((time, _, kind))) = heap.pop() {
        match kind {
            EventKind::Arrival { index } => {
                let request = config.requests[index];
                let pool = pool_for(request.class);
                if pools[pool].free > 0 {
                    pools[pool].free -= 1;
                    heap.push(std::cmp::Reverse((
                        time + request.service_ms,
                        1_000_000 + index as u64,
                        EventKind::Complete { pool, index },
                    )));
                } else {
                    pools[pool].waiting.push_back(index);
                }
            }
            EventKind::Complete { pool, index } => {
                let request = config.requests[index];
                outcome.completions.push(SimCompletion {
                    request_id: request.id,
                    class: request.class,
                    latency: Duration::from_millis(time - request.arrival_ms),
                });
                if let Some(next) = pools[pool].waiting.pop_front() {
                    let next_request = config.requests[next];
                    heap.push(std::cmp::Reverse((
                        time + next_request.service_ms,
                        1_000_000 + next as u64,
                        EventKind::Complete { pool, index: next },
                    )));
                } else {
                    pools[pool].free += 1;
                }
            }
        }
    }
    outcome
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::node::{Capacity, NodeRole, NodeSelector};
    use crate::policy::QueueAffinityRule;
    use crate::queue::QueueName;

    #[test]
    fn percentile_uses_nearest_rank() {
        let values: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&values, 50), Duration::from_millis(50));
        assert_eq!(percentile(&values, 95), Duration::from_millis(95));
        assert_eq!(percentile(&values, 100), Duration::from_millis(100));
        assert_eq!(percentile(&[], 95), Duration::ZERO);
    }

    #[test]
    fn a_single_worker_pool_is_fifo() {
        let config = SimConfig {
            nodes: vec![NodeProfile::from_topology(
                "a",
                vec![NodeRole::Interactive],
                Vec::new(),
                Capacity::new(1, 100),
            )],
            requests: vec![
                SimRequest {
                    id: 1,
                    class: WorkloadClass::Interactive,
                    arrival_ms: 0,
                    service_ms: 10,
                },
                SimRequest {
                    id: 2,
                    class: WorkloadClass::Interactive,
                    arrival_ms: 0,
                    service_ms: 10,
                },
            ],
            policy: RoutingPolicy::default(),
            routing: false,
        };
        let outcome = simulate(&config);
        let latencies = outcome.latencies_for(WorkloadClass::Interactive);
        assert_eq!(latencies.len(), 2);
        assert_eq!(p95(&latencies), Duration::from_millis(20));
    }

    #[test]
    fn routing_keeps_classes_in_separate_pools() {
        let policy = RoutingPolicy {
            queue_affinity: vec![
                QueueAffinityRule {
                    selector: NodeSelector::roles(vec![NodeRole::Interactive]),
                    queues: vec![QueueName::new("interactive")],
                },
                QueueAffinityRule {
                    selector: NodeSelector::roles(vec![NodeRole::Report]),
                    queues: vec![QueueName::new("reports")],
                },
            ],
            ..RoutingPolicy::default()
        };
        let config = SimConfig {
            nodes: vec![
                NodeProfile::from_topology(
                    "a",
                    vec![NodeRole::Interactive],
                    Vec::new(),
                    Capacity::new(1, 100),
                ),
                NodeProfile::from_topology(
                    "c",
                    vec![NodeRole::Report],
                    Vec::new(),
                    Capacity::new(1, 100),
                ),
            ],
            requests: vec![
                SimRequest {
                    id: 1,
                    class: WorkloadClass::Report,
                    arrival_ms: 0,
                    service_ms: 100,
                },
                SimRequest {
                    id: 2,
                    class: WorkloadClass::Interactive,
                    arrival_ms: 0,
                    service_ms: 5,
                },
            ],
            policy,
            routing: true,
        };
        let outcome = simulate(&config);
        // The interactive request never waits behind the report.
        assert_eq!(
            outcome.latencies_for(WorkloadClass::Interactive),
            vec![Duration::from_millis(5)]
        );
    }
}
