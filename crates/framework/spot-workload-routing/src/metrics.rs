//! Metrics: what the router did, for the host to publish (ADR-24).
//!
//! Every fallback the brief requires — an empty target pool, a spent hop budget, a replica refused
//! for lag or for read-your-writes — is recorded as a [`RoutingEvent`]. The crate does not depend
//! on a telemetry stack; it defines the sink [`Metrics`] and a counting implementation
//! ([`CountingMetrics`]) a host or a test uses.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::class::WorkloadClass;
use crate::node::NodeId;

/// Which of the three decisions was taken, without the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DecisionKind {
    /// Served on the local node.
    Local,
    /// Forwarded to another node.
    Forward,
    /// Served locally from a replica.
    Replica,
}

/// One routing event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutingEvent {
    /// A request was routed.
    Routed {
        /// The class the router assigned.
        class: WorkloadClass,
        /// The decision taken.
        decision: DecisionKind,
    },
    /// A path rule matched but no node carried its target labels.
    PoolEmpty {
        /// The class the router assigned.
        class: WorkloadClass,
    },
    /// A forward was refused because the hop budget is spent.
    HopLimit {
        /// The node the forward wanted.
        target: NodeId,
    },
    /// A read fell back to the primary.
    ReplicaFallback {
        /// Why, as a [`crate::codes`] constant.
        reason: &'static str,
    },
    /// A request was forwarded.
    Forwarded {
        /// The node it went to.
        target: NodeId,
    },
    /// The transport failed to forward.
    ForwardFailed {
        /// The node the forward wanted.
        target: NodeId,
    },
}

impl RoutingEvent {
    /// A stable, non-dotted counter name for this event. (Dotted names are reserved for the error
    /// catalogue, so metrics use plain words.)
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            RoutingEvent::Routed { .. } => "routed",
            RoutingEvent::PoolEmpty { .. } => "pool_empty",
            RoutingEvent::HopLimit { .. } => "hop_limit",
            RoutingEvent::ReplicaFallback { .. } => "replica_fallback",
            RoutingEvent::Forwarded { .. } => "forwarded",
            RoutingEvent::ForwardFailed { .. } => "forward_failed",
        }
    }
}

/// A sink for [`RoutingEvent`]s. The host wires its telemetry here; a test uses
/// [`CountingMetrics`].
pub trait Metrics: Send + Sync {
    /// Record one event. Implementations must never block a request for long and never panic.
    fn record(&self, event: RoutingEvent);
}

/// A sink that discards everything, for a host that has not wired telemetry yet.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoMetrics;

impl Metrics for NoMetrics {
    fn record(&self, _event: RoutingEvent) {}
}

/// A sink that counts events by [`RoutingEvent::kind`], for tests and for a lightweight host.
#[derive(Debug, Default)]
pub struct CountingMetrics {
    counters: Mutex<BTreeMap<&'static str, u64>>,
}

impl CountingMetrics {
    /// An empty counter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many events of a kind were recorded.
    #[must_use]
    pub fn count(&self, kind: &str) -> u64 {
        match self.counters.lock() {
            Ok(guard) => guard.get(kind).copied().unwrap_or(0),
            Err(poisoned) => poisoned.into_inner().get(kind).copied().unwrap_or(0),
        }
    }

    /// Every counter, in name order.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<&'static str, u64> {
        match self.counters.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl Metrics for CountingMetrics {
    fn record(&self, event: RoutingEvent) {
        if let Ok(mut guard) = self.counters.lock() {
            *guard.entry(event.kind()).or_insert(0) += 1;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn counting_metrics_counts_by_kind() {
        let metrics = CountingMetrics::new();
        metrics.record(RoutingEvent::Routed {
            class: WorkloadClass::Interactive,
            decision: DecisionKind::Local,
        });
        metrics.record(RoutingEvent::Routed {
            class: WorkloadClass::Report,
            decision: DecisionKind::Forward,
        });
        metrics.record(RoutingEvent::PoolEmpty {
            class: WorkloadClass::Report,
        });
        assert_eq!(metrics.count("routed"), 2);
        assert_eq!(metrics.count("pool_empty"), 1);
        assert_eq!(metrics.count("hop_limit"), 0);
    }

    #[test]
    fn no_metrics_never_panics() {
        NoMetrics.record(RoutingEvent::Forwarded {
            target: NodeId::new("n1"),
        });
    }
}
