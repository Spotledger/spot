//! The `spot-http` tower layer (WP-PLT-20.T1): a report request is forwarded, a local request
//! carries its decision and read preference, and the hop budget is enforced.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use http::{Request, Response};
use spot_workload_routing::class::WorkloadClass;
use spot_workload_routing::forward::RecordingTransport;
use spot_workload_routing::metrics::{CountingMetrics, NoMetrics};
use spot_workload_routing::middleware::{MiddlewareError, NoSessions, RoutingLayer};
use spot_workload_routing::node::{Capacity, NodeId, NodeProfile, NodeRole, NodeSelector};
use spot_workload_routing::policy::{
    PathMatcher, PathRule, PolicyStore, QueueAffinityRule, ReplicaReadRule, RoutingPolicy,
};
use spot_workload_routing::queue::QueueName;
use spot_workload_routing::read::{ReadPreference, ReplicaStatus, SessionToken};
use spot_workload_routing::route::{RouteOutcome, RoutingEngine};
use time::OffsetDateTime;
use tower::{Layer, ServiceExt, service_fn};

fn topology() -> Vec<NodeProfile> {
    vec![
        NodeProfile::from_topology(
            "a",
            vec![NodeRole::Interactive],
            Vec::new(),
            Capacity::new(4, 100),
        ),
        NodeProfile::from_topology(
            "c",
            vec![NodeRole::Report],
            Vec::new(),
            Capacity::new(2, 100),
        ),
    ]
}

fn policy() -> RoutingPolicy {
    RoutingPolicy {
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
        path_rules: vec![PathRule {
            matcher: PathMatcher::prefix("/api/v1/report/"),
            target: NodeSelector::roles(vec![NodeRole::Report]),
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

fn engine(policy: RoutingPolicy, replicas: Vec<ReplicaStatus>) -> Arc<RoutingEngine> {
    let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    Arc::new(RoutingEngine::new(
        Arc::new(PolicyStore::new(policy)),
        topology(),
        replicas,
        NodeId::new("a"),
        Arc::new(NoMetrics),
        Arc::new(move || at),
    ))
}

fn layer(
    engine: Arc<RoutingEngine>,
    transport: Arc<RecordingTransport>,
    max_hops: u32,
) -> RoutingLayer<RecordingTransport> {
    RoutingLayer::new(engine, Arc::new(NoSessions), transport, max_hops)
}

#[tokio::test]
async fn a_report_request_is_forwarded_to_the_report_node() {
    let transport = Arc::new(RecordingTransport::new());
    let inner = service_fn(|_req: Request<String>| async {
        Ok::<_, Infallible>(Response::new("local".to_string()))
    });
    let service = layer(engine(policy(), Vec::new()), Arc::clone(&transport), 4).layer(inner);

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/report/sales/run")
        .body(String::new())
        .unwrap();
    let response = service.oneshot(request).await.unwrap();
    assert_eq!(response.body(), "forwarded to c");

    let forwarded = transport.forwarded();
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].target, NodeId::new("c"));
    assert_eq!(forwarded[0].hops, 1);
}

#[tokio::test]
async fn a_local_request_carries_its_decision_and_read_preference() {
    let seen: Arc<Mutex<Option<(RouteOutcome, ReadPreference)>>> = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&seen);
    let inner = service_fn(move |req: Request<String>| {
        let captured = Arc::clone(&captured);
        async move {
            let outcome = req.extensions().get::<RouteOutcome>().cloned();
            let read = req.extensions().get::<ReadPreference>().copied();
            if let (Some(outcome), Some(read)) = (outcome, read) {
                *captured.lock().unwrap() = Some((outcome, read));
            }
            Ok::<_, Infallible>(Response::new("local".to_string()))
        }
    });
    let transport = Arc::new(RecordingTransport::new());
    let service = layer(engine(policy(), Vec::new()), transport, 4).layer(inner);

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/document/sales-order")
        .body(String::new())
        .unwrap();
    let response = service.oneshot(request).await.unwrap();
    assert_eq!(response.body(), "local");

    let (outcome, read) = seen.lock().unwrap().clone().expect("captured");
    assert!(outcome.decision.is_local());
    assert_eq!(read, ReadPreference::Primary);
}

#[tokio::test]
async fn a_write_on_a_replica_eligible_path_reads_the_primary() {
    let replica = ReplicaStatus {
        id: NodeId::new("r1"),
        applied_token: SessionToken(10),
        lag: std::time::Duration::from_millis(20),
    };
    let seen: Arc<Mutex<Option<ReadPreference>>> = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&seen);
    let inner = service_fn(move |req: Request<String>| {
        let captured = Arc::clone(&captured);
        async move {
            if let Some(read) = req.extensions().get::<ReadPreference>().copied() {
                *captured.lock().unwrap() = Some(read);
            }
            Ok::<_, Infallible>(Response::new("local".to_string()))
        }
    });
    // Serve on the report node so the request is local and the replica rule can apply.
    let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let engine = Arc::new(RoutingEngine::new(
        Arc::new(PolicyStore::new(policy())),
        topology(),
        vec![replica],
        NodeId::new("c"),
        Arc::new(NoMetrics),
        Arc::new(move || at),
    ));
    let transport = Arc::new(RecordingTransport::new());
    let service = layer(engine, transport, 4).layer(inner);

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/report/sales/run")
        .body(String::new())
        .unwrap();
    let _ = service.oneshot(request).await.unwrap();
    assert_eq!(seen.lock().unwrap().clone(), Some(ReadPreference::Primary));
}

#[tokio::test]
async fn the_hop_budget_is_enforced_by_the_layer() {
    // The rule allows ten hops, but the layer's own budget is one; a request that already took one
    // hop must not be forwarded again.
    let mut policy = policy();
    policy.path_rules[0].max_hops = Some(10);
    let transport = Arc::new(RecordingTransport::new());
    let inner = service_fn(|_req: Request<String>| async {
        Ok::<_, Infallible>(Response::new("local".to_string()))
    });
    let service = layer(engine(policy, Vec::new()), Arc::clone(&transport), 1).layer(inner);

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/report/sales/run")
        .header("x-spot-routing-hops", "1")
        .body(String::new())
        .unwrap();
    let result = service.oneshot(request).await;
    match result {
        Err(MiddlewareError::Forward(error)) => {
            assert_eq!(error.code(), spot_workload_routing::codes::HOP_LIMIT);
        }
        other => panic!("expected a hop-limit error, got {other:?}"),
    }
    assert!(transport.forwarded().is_empty());
}

#[tokio::test]
async fn the_deadline_header_is_propagated() {
    let transport = Arc::new(RecordingTransport::new());
    let inner = service_fn(|_req: Request<String>| async {
        Ok::<_, Infallible>(Response::new("local".to_string()))
    });
    let service = layer(engine(policy(), Vec::new()), Arc::clone(&transport), 4).layer(inner);

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/report/sales/run")
        .header("x-spot-deadline-ms", "1750")
        .body(String::new())
        .unwrap();
    let _ = service.oneshot(request).await.unwrap();
    let forwarded = transport.forwarded();
    assert_eq!(forwarded[0].deadline_ms, Some(1_750));
}

#[tokio::test]
async fn metrics_count_a_forward() {
    let metrics = Arc::new(CountingMetrics::new());
    let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let engine = Arc::new(RoutingEngine::new(
        Arc::new(PolicyStore::new(policy())),
        topology(),
        Vec::new(),
        NodeId::new("a"),
        metrics.clone(),
        Arc::new(move || at),
    ));
    let transport = Arc::new(RecordingTransport::new());
    let inner = service_fn(|_req: Request<String>| async {
        Ok::<_, Infallible>(Response::new("local".to_string()))
    });
    let service = RoutingLayer::new(engine, Arc::new(NoSessions), transport, 4).layer(inner);
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/report/sales/run")
        .body(String::new())
        .unwrap();
    let _ = service.oneshot(request).await.unwrap();
    assert_eq!(metrics.count("forwarded"), 1);
}
