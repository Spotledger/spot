//! The routing decision: where a request runs and where its reads go (ADR-24).
//!
//! [`route`] is the whole engine as one pure function: given a request's metadata, the topology,
//! the live policy, the replica set and a clock, it returns a [`RouteOutcome`]. [`RoutingEngine`]
//! wraps it with a [`PolicyStore`], the metrics sink and the host's clock, so the HTTP middleware
//! has one object to call.

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use crate::class::WorkloadClass;
use crate::metrics::{DecisionKind, Metrics, RoutingEvent};
use crate::node::{NodeId, NodeProfile};
use crate::policy::{PolicyStore, RoutingPolicy};
use crate::queue::QueueName;
use crate::read::{ReadDecision, ReadPreference, ReplicaStatus, SessionReadState, choose_read};

/// Everything the router needs to know about a request, and nothing more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMeta {
    /// The request path (no query string).
    pub path: String,
    /// The route name the native API resolved, when there is one.
    pub route: Option<String>,
    /// The request's own workload class, before any path rule overrides it.
    pub class: WorkloadClass,
    /// Whether the request carries a write. A write never reads a replica.
    pub is_write: bool,
    /// How many times the request has already been forwarded.
    pub hops: u32,
    /// The remaining end-to-end budget, when the caller set one.
    pub deadline: Option<crate::forward::Deadline>,
    /// The caller session's read-your-writes state.
    pub session: SessionReadState,
}

impl RequestMeta {
    /// Metadata for a request to a path with a class; nothing is a write, no session is known.
    #[must_use]
    pub fn new(path: impl Into<String>, class: WorkloadClass) -> Self {
        Self {
            path: path.into(),
            route: None,
            class,
            is_write: false,
            hops: 0,
            deadline: None,
            session: SessionReadState::none(),
        }
    }

    /// The same metadata, marked as carrying a write.
    #[must_use]
    pub fn write(mut self) -> Self {
        self.is_write = true;
        self
    }

    /// The same metadata with a route name.
    #[must_use]
    pub fn with_route(mut self, route: impl Into<String>) -> Self {
        self.route = Some(route.into());
        self
    }

    /// The same metadata with a hop count.
    #[must_use]
    pub fn with_hops(mut self, hops: u32) -> Self {
        self.hops = hops;
        self
    }

    /// The same metadata with a deadline.
    #[must_use]
    pub fn with_deadline(mut self, deadline: crate::forward::Deadline) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// The same metadata with a session state.
    #[must_use]
    pub fn with_session(mut self, session: SessionReadState) -> Self {
        self.session = session;
        self
    }
}

/// Where a request runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteDecision {
    /// Serve it here.
    Local,
    /// Forward it to this node.
    Forward(NodeId),
    /// Serve it here, reading from a replica within `max_lag`.
    Replica {
        /// The maximum acceptable replication lag.
        max_lag: Duration,
    },
}

impl RouteDecision {
    /// Whether the request is served locally (with or without a replica read).
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, RouteDecision::Local | RouteDecision::Replica { .. })
    }

    /// The forward target, when the decision forwards.
    #[must_use]
    pub fn target(&self) -> Option<&NodeId> {
        match self {
            RouteDecision::Forward(node) => Some(node),
            _ => None,
        }
    }

    /// The coarse kind, for metrics.
    #[must_use]
    pub fn kind(&self) -> DecisionKind {
        match self {
            RouteDecision::Local => DecisionKind::Local,
            RouteDecision::Forward(_) => DecisionKind::Forward,
            RouteDecision::Replica { .. } => DecisionKind::Replica,
        }
    }
}

/// The result of a routing decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteOutcome {
    /// Where the request runs.
    pub decision: RouteDecision,
    /// The workload class the router assigned (a path rule may override the request's own).
    pub class: WorkloadClass,
    /// Where the reads go.
    pub read: ReadDecision,
    /// The stable code of a fallback, when the router fell back to local.
    pub fallback: Option<&'static str>,
}

impl RouteOutcome {
    /// Whether the router fell back to local because it could not use a rule's target.
    #[must_use]
    pub fn fell_back(&self) -> bool {
        self.fallback.is_some()
    }
}

/// Choose the target node for a class among candidates: a node that serves the class first, then
/// more workers, then a shallower queue, then the lower id. Deterministic, so a test can predict
/// the choice.
fn choose_node<'a>(
    candidates: &[&'a NodeProfile],
    class: WorkloadClass,
    policy: &RoutingPolicy,
) -> Option<&'a NodeProfile> {
    candidates.iter().copied().min_by_key(|node| {
        (
            !policy.node_serves(node, class),
            std::cmp::Reverse(node.capacity.workers),
            node.capacity.max_queue_depth,
            node.id.clone(),
        )
    })
}

/// Route one request.
///
/// A path rule that targets nodes other than the local one produces [`RouteDecision::Forward`]; a
/// spent hop budget or an empty target pool falls back to [`RouteDecision::Local`] with the reason
/// in [`RouteOutcome::fallback`]. A read-only, staleness-tolerant request with a matching
/// replica-read rule produces [`RouteDecision::Replica`] when [`choose_read`] allows it; a write
/// never does.
#[must_use]
pub fn route(
    meta: &RequestMeta,
    nodes: &[NodeProfile],
    policy: &RoutingPolicy,
    replicas: &[ReplicaStatus],
    local: &NodeId,
    now: OffsetDateTime,
) -> RouteOutcome {
    let rule = policy.path_rule_for(&meta.path, meta.route.as_deref());
    let class = rule.and_then(|r| r.class).unwrap_or(meta.class);

    let mut decision = RouteDecision::Local;
    let mut fallback: Option<&'static str> = None;

    if let Some(rule) = rule
        && !rule.target.is_empty()
    {
        let local_matches = nodes
            .iter()
            .find(|node| node.id == *local)
            .is_some_and(|node| rule.target.matches(node));
        if !local_matches {
            let candidates: Vec<&NodeProfile> = nodes
                .iter()
                .filter(|node| rule.target.matches(node))
                .collect();
            let max_hops = rule.max_hops.unwrap_or(policy.max_hops);
            if candidates.is_empty() {
                fallback = Some(crate::codes::NO_TARGET);
            } else if meta.hops >= max_hops {
                fallback = Some(crate::codes::HOP_LIMIT);
            } else if let Some(target) = choose_node(&candidates, class, policy) {
                decision = RouteDecision::Forward(target.id.clone());
            } else {
                fallback = Some(crate::codes::NO_TARGET);
            }
        }
    }

    // Read preference. A write always reads the primary; a forwarded request leaves the choice to
    // the target.
    let mut read = if meta.is_write {
        ReadDecision {
            preference: ReadPreference::Primary,
            replica: None,
            reason: Some(crate::codes::WRITE_TO_REPLICA),
        }
    } else if let Some(rule) = policy.replica_rule_for(&meta.path, meta.route.as_deref()) {
        choose_read(
            false,
            rule.max_lag(),
            replicas,
            &meta.session,
            policy.read_your_writes_window(),
            now,
        )
    } else {
        ReadDecision {
            preference: ReadPreference::Primary,
            replica: None,
            reason: None,
        }
    };

    if let RouteDecision::Forward(_) = decision {
        read = ReadDecision {
            preference: ReadPreference::Primary,
            replica: None,
            reason: None,
        };
    } else if let ReadPreference::Replica { max_lag } = read.preference {
        decision = RouteDecision::Replica { max_lag };
    }

    RouteOutcome {
        decision,
        class,
        read,
        fallback,
    }
}

/// The engine the middleware calls: a live policy, the topology, the replica set, the metrics sink
/// and the host's clock.
pub struct RoutingEngine {
    policy: Arc<PolicyStore>,
    nodes: Vec<NodeProfile>,
    replicas: Vec<ReplicaStatus>,
    local: NodeId,
    metrics: Arc<dyn Metrics>,
    clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

impl RoutingEngine {
    /// An engine over its parts. `clock` is injected: this crate never reads the wall clock.
    #[must_use]
    pub fn new(
        policy: Arc<PolicyStore>,
        nodes: Vec<NodeProfile>,
        replicas: Vec<ReplicaStatus>,
        local: NodeId,
        metrics: Arc<dyn Metrics>,
        clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    ) -> Self {
        Self {
            policy,
            nodes,
            replicas,
            local,
            metrics,
            clock,
        }
    }

    /// The live policy snapshot.
    #[must_use]
    pub fn policy(&self) -> Arc<RoutingPolicy> {
        self.policy.load()
    }

    /// The local node id.
    #[must_use]
    pub fn local(&self) -> &NodeId {
        &self.local
    }

    /// The queues any node's workers consume — what `spot-jobs` worker startup asks for when it
    /// starts the workers on `node`.
    #[must_use]
    pub fn queues_for(&self, node: &NodeProfile) -> Vec<QueueName> {
        self.policy.load().queues_for(node)
    }

    /// The queues the local node's workers consume — what `spot-jobs` worker startup asks for.
    #[must_use]
    pub fn queues_for_local(&self) -> Vec<QueueName> {
        match self.nodes.iter().find(|node| node.id == self.local) {
            Some(node) => self.queues_for(node),
            None => vec![QueueName::new("default")],
        }
    }

    /// Decide where a request runs, recording the outcome and every fallback as a metric.
    #[must_use]
    pub fn decide(&self, meta: &RequestMeta) -> RouteOutcome {
        let policy = self.policy.load();
        let now = (self.clock)();
        let outcome = route(meta, &self.nodes, &policy, &self.replicas, &self.local, now);
        self.metrics.record(RoutingEvent::Routed {
            class: outcome.class,
            decision: outcome.decision.kind(),
        });
        if let Some(reason) = outcome.fallback {
            match reason {
                crate::codes::NO_TARGET => self.metrics.record(RoutingEvent::PoolEmpty {
                    class: outcome.class,
                }),
                crate::codes::HOP_LIMIT => {
                    if let Some(target) = outcome.decision.target().cloned() {
                        self.metrics.record(RoutingEvent::HopLimit { target });
                    }
                }
                _ => {}
            }
        }
        if let Some(target) = outcome.decision.target() {
            self.metrics.record(RoutingEvent::Forwarded {
                target: target.clone(),
            });
        }
        if let Some(reason) = outcome.read.reason {
            self.metrics
                .record(RoutingEvent::ReplicaFallback { reason });
        }
        outcome
    }

    /// Record that a forward to `target` failed, so the host can alert on it.
    pub fn record_forward_failed(&self, target: &NodeId) {
        self.metrics.record(RoutingEvent::ForwardFailed {
            target: target.clone(),
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::node::{Capacity, NodeRole};
    use crate::policy::{PathMatcher, PathRule, QueueAffinityRule, ReplicaReadRule};
    use crate::read::SessionToken;

    fn node(id: &str, roles: Vec<NodeRole>, workers: u32) -> NodeProfile {
        NodeProfile::from_topology(id, roles, Vec::new(), Capacity::new(workers, 100))
    }

    fn topology() -> Vec<NodeProfile> {
        vec![
            node("a", vec![NodeRole::Interactive], 8),
            node("b", vec![NodeRole::Interactive], 8),
            node("c", vec![NodeRole::Report], 4),
        ]
    }

    fn policy() -> RoutingPolicy {
        RoutingPolicy {
            queue_affinity: vec![
                QueueAffinityRule {
                    selector: crate::node::NodeSelector::roles(vec![NodeRole::Interactive]),
                    queues: vec![QueueName::new("interactive")],
                },
                QueueAffinityRule {
                    selector: crate::node::NodeSelector::roles(vec![NodeRole::Report]),
                    queues: vec![QueueName::new("reports")],
                },
            ],
            path_rules: vec![PathRule {
                matcher: PathMatcher::prefix("/api/v1/report/"),
                target: crate::node::NodeSelector::roles(vec![NodeRole::Report]),
                class: Some(WorkloadClass::Report),
                max_hops: None,
            }],
            replica_reads: vec![ReplicaReadRule {
                matcher: PathMatcher::prefix("/api/v1/report/"),
                max_lag_ms: 5_000,
                staleness_tolerant: true,
            }],
            ..RoutingPolicy::default()
        }
    }

    fn at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp")
    }

    #[test]
    fn interactive_request_stays_local() {
        let meta = RequestMeta::new("/api/v1/document/sales-order", WorkloadClass::Interactive);
        let outcome = route(&meta, &topology(), &policy(), &[], &NodeId::new("a"), at());
        assert_eq!(outcome.decision, RouteDecision::Local);
        assert!(!outcome.fell_back());
        assert_eq!(outcome.read.preference, ReadPreference::Primary);
    }

    #[test]
    fn report_request_forwards_to_the_report_node() {
        let meta = RequestMeta::new("/api/v1/report/sales/run", WorkloadClass::Interactive);
        let outcome = route(&meta, &topology(), &policy(), &[], &NodeId::new("a"), at());
        assert_eq!(outcome.decision, RouteDecision::Forward(NodeId::new("c")));
        assert_eq!(outcome.class, WorkloadClass::Report);
    }

    #[test]
    fn report_node_serves_the_report_locally() {
        let meta = RequestMeta::new("/api/v1/report/sales/run", WorkloadClass::Interactive);
        let outcome = route(&meta, &topology(), &policy(), &[], &NodeId::new("c"), at());
        assert!(outcome.decision.is_local());
        assert_eq!(outcome.class, WorkloadClass::Report);
    }

    #[test]
    fn empty_pool_falls_back_to_local_with_a_reason() {
        let mut policy = policy();
        policy.path_rules[0].target = crate::node::NodeSelector::labels(vec!["nowhere".to_owned()]);
        let meta = RequestMeta::new("/api/v1/report/sales/run", WorkloadClass::Report);
        let outcome = route(&meta, &topology(), &policy, &[], &NodeId::new("a"), at());
        assert_eq!(outcome.decision, RouteDecision::Local);
        assert_eq!(outcome.fallback, Some(crate::codes::NO_TARGET));
    }

    #[test]
    fn spent_hop_budget_falls_back_to_local() {
        let meta = RequestMeta::new("/api/v1/report/sales/run", WorkloadClass::Report)
            .with_hops(RoutingPolicy::default().max_hops);
        let outcome = route(&meta, &topology(), &policy(), &[], &NodeId::new("a"), at());
        assert_eq!(outcome.decision, RouteDecision::Local);
        assert_eq!(outcome.fallback, Some(crate::codes::HOP_LIMIT));
    }

    #[test]
    fn replica_read_is_used_for_a_staleness_tolerant_read() {
        let replica = ReplicaStatus {
            id: NodeId::new("r1"),
            applied_token: SessionToken(10),
            lag: Duration::from_millis(50),
        };
        // On the report node itself, a report read may use the replica.
        let meta = RequestMeta::new("/api/v1/report/sales/run", WorkloadClass::Report);
        let outcome = route(
            &meta,
            &topology(),
            &policy(),
            &[replica],
            &NodeId::new("c"),
            at(),
        );
        assert!(matches!(outcome.decision, RouteDecision::Replica { .. }));
        assert!(outcome.read.preference.is_replica());
    }

    #[test]
    fn a_write_never_reads_a_replica() {
        let replica = ReplicaStatus {
            id: NodeId::new("r1"),
            applied_token: SessionToken(10),
            lag: Duration::from_millis(50),
        };
        let meta = RequestMeta::new("/api/v1/report/sales/run", WorkloadClass::Report).write();
        let outcome = route(
            &meta,
            &topology(),
            &policy(),
            &[replica],
            &NodeId::new("c"),
            at(),
        );
        assert_eq!(outcome.read.preference, ReadPreference::Primary);
        assert_eq!(outcome.read.reason, Some(crate::codes::WRITE_TO_REPLICA));
        assert!(!matches!(outcome.decision, RouteDecision::Replica { .. }));
    }

    #[test]
    fn engine_records_metrics() {
        let metrics = Arc::new(crate::metrics::CountingMetrics::new());
        let engine = RoutingEngine::new(
            Arc::new(PolicyStore::new(policy())),
            topology(),
            Vec::new(),
            NodeId::new("a"),
            metrics.clone(),
            Arc::new(at),
        );
        let _ = engine.decide(&RequestMeta::new(
            "/api/v1/report/sales/run",
            WorkloadClass::Report,
        ));
        assert_eq!(metrics.count("routed"), 1);
        assert_eq!(metrics.count("forwarded"), 1);
    }
}
