//! Workload routing, hosted (WP-PLT-20.T2, ADR-24): queue affinity per node, path-based routing
//! of report requests to dedicated nodes, and staleness-bounded replica reads.
//!
//! `spot-workload-routing` (WP-PLT-20.T1) is the pure policy engine: given a topology, a policy
//! document and a request's metadata, [`spot_workload_routing::route::route`] decides where the
//! request runs and where its reads go. This module is the host: it reads
//! `[services.workload_routing]`, builds the engine from it, and applies the decision to real
//! requests with a `tower`/`axum` layer.
//!
//! # The decision this module applies
//!
//! - **Queue affinity.** [`QueueAffinityPort`] exposes the queues this node's workers should
//!   consume, from the node's declared roles, through the standard role → queue mapping
//!   ([`spot_workload_routing::policy::RoutingPolicy::standard`]). **Advisory today**: the job
//!   worker (`crate::services::jobs`) does not read this port yet — see the task's Open
//!   questions — so it still claims the queues `services.jobs.queues` names regardless of what
//!   this port answers.
//! - **Report routing.** A request under `/api/v1/report/` is served locally when this node
//!   carries the `report` role; otherwise the chosen library convention is an HTTP `307
//!   Temporary Redirect` to the owning node's configured `host` (never a transparent proxy): the
//!   caller's own client re-requests the target directly, so it re-authenticates there under the
//!   target's own authentication, exactly as it would for any other node — no credential is
//!   read, held or forwarded by this module. A target role with no node that has a configured
//!   `host` is *unavailable*; the request then falls back to local execution only when
//!   `fallback_to_local` is `true`, and is refused with `503` otherwise.
//! - **Replica reads.** Only the reports named in `replica_reports` may read a replica, each
//!   within its own `max_lag_ms`; a replica whose lag exceeds that bound, or whose absence
//!   leaves no replica at all, falls back to the primary ([`spot_workload_routing::read::choose_read`]).
//!   The chosen [`spot_workload_routing::read::ReadPreference`] is attached to the request's
//!   extensions for a storage adapter to honour; `spot-postgres` does not read it yet (an open
//!   question of WP-PLT-20.T1, unchanged here).
//!
//! # What this module does not do
//!
//! It never proxies a request's body or headers to another node itself (the redirect means the
//! caller's own client makes the next hop); it never reads the wall clock (the site's injected
//! [`spot_core::Clock`] is the only source of `now`, passed to the engine as a closure); and it
//! never logs or records a credential, a header value or a request body — only the routing
//! outcome's coarse shape (decision kind, target node id, class) if a caller wires telemetry.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use spot_http::api::ports::routing::QueueAffinityPort;
use spot_workload_routing::metrics::{Metrics, NoMetrics};
use spot_workload_routing::middleware::{NoSessions, meta_from_parts};
use spot_workload_routing::node::{Capacity, NodeId, NodeProfile, NodeRole};
use spot_workload_routing::policy::{PathMatcher, PolicyStore, ReplicaReadRule, RoutingPolicy};
use spot_workload_routing::queue::QueueName;
use spot_workload_routing::read::{ReadPreference, ReplicaStatus, SessionToken};
use spot_workload_routing::route::{RouteDecision, RouteOutcome, RoutingEngine};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after every other hosting module, so the router edit below wraps every route the
/// site mounted (a `router_layer` edit's `.layer()` only wraps what was merged into the router
/// before it runs; registering last means every earlier module's routes, report's included, are
/// already merged).
pub const ORDER: u32 = 1990;

/// The node id a site with no `[services.workload_routing]` table runs as: a single node that
/// serves everything locally (routing stays a no-op until nodes are configured).
const DEFAULT_NODE_ID: &str = "local";

// ---- settings ----

/// `[services.workload_routing]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the routing policy is built and applied at all. Off, every request is served
    /// locally and no replica is ever used, exactly as before this module existed.
    pub enabled: bool,
    /// This node's id; must name one entry of `nodes`.
    pub local_node: String,
    /// The topology: every node routing may target, including this one.
    pub nodes: Vec<NodeSettings>,
    /// The replica pool's live status, as the operator (or a monitor) reports it. Empty means no
    /// replica is ever used: every replica-eligible read falls back to the primary.
    pub replicas: Vec<ReplicaSettings>,
    /// The reports allowed to read a replica (`replica_ok`), each with its own staleness bound.
    /// A report not named here never reads a replica, however close the nearest one's lag is.
    pub replica_reports: Vec<ReplicaReportSettings>,
    /// Whether a request routed to an unavailable target (a role with no node that has a
    /// configured `host`) runs here instead of being refused.
    pub fallback_to_local: bool,
    /// The forward hop budget (unused today: this module answers a redirect, never forwards a
    /// request itself, but the budget still bounds a chain of redirects a client could follow).
    pub max_hops: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            local_node: DEFAULT_NODE_ID.to_owned(),
            nodes: vec![NodeSettings::local_default()],
            replicas: Vec::new(),
            replica_reports: Vec::new(),
            fallback_to_local: true,
            max_hops: spot_workload_routing::policy::DEFAULT_MAX_HOPS,
        }
    }
}

/// One node of the topology.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NodeSettings {
    /// The node's id.
    pub id: String,
    /// The roles the node serves (`interactive`, `gateway`, `report`, `ai`, `bulk`).
    pub roles: Vec<String>,
    /// Free-form labels a path rule may target.
    pub labels: Vec<String>,
    /// Concurrent job workers.
    pub workers: u32,
    /// The queue depth beyond which the node is considered saturated.
    pub max_queue_depth: u32,
    /// The base URL this node answers on (`https://report-1.internal:8443`), used only to build
    /// the `Location` of a redirect to it. A node with no `host` is never a redirect target: a
    /// path rule that would choose it is *unavailable* instead (see [`Settings::fallback_to_local`]).
    pub host: Option<String>,
}

impl Default for NodeSettings {
    fn default() -> Self {
        Self {
            id: String::new(),
            roles: Vec::new(),
            labels: Vec::new(),
            workers: 1,
            max_queue_depth: 1000,
            host: None,
        }
    }
}

impl NodeSettings {
    fn local_default() -> Self {
        Self {
            id: DEFAULT_NODE_ID.to_owned(),
            roles: vec!["interactive".to_owned()],
            ..Self::default()
        }
    }
}

/// One replica's live status.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReplicaSettings {
    /// The replica's id (for logs; never a connection string).
    pub id: String,
    /// How far behind the primary the replica is, in milliseconds.
    pub lag_ms: u64,
    /// The highest commit token the replica has applied (`0` when unknown; a session that has
    /// just written is then never considered caught up, which is the safe side).
    pub applied_token: u64,
}

/// One report allow-listed for replica reads.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReplicaReportSettings {
    /// The report's storage name (matches `/api/v1/report/{report}/run`).
    pub report: String,
    /// The maximum acceptable replication lag for this report's reads, in milliseconds.
    pub max_lag_ms: u64,
}

impl Default for ReplicaReportSettings {
    fn default() -> Self {
        Self {
            report: String::new(),
            max_lag_ms: 5_000,
        }
    }
}

// ---- building the engine from settings ----

fn node_roles(id: &str, raw: &[String]) -> Result<Vec<NodeRole>> {
    raw.iter()
        .map(|name| {
            NodeRole::parse(name)
                .map_err(|e| Error(format!("services.workload_routing: node `{id}`: {e}")))
        })
        .collect()
}

fn node_profile(cfg: &NodeSettings) -> Result<NodeProfile> {
    if cfg.id.is_empty() {
        return Err(Error(
            "services.workload_routing.nodes: a node has no id".to_owned(),
        ));
    }
    let roles = node_roles(&cfg.id, &cfg.roles)?;
    let profile = NodeProfile::from_topology(
        NodeId::new(cfg.id.clone()),
        roles,
        cfg.labels.clone(),
        Capacity::new(cfg.workers.max(1), cfg.max_queue_depth),
    );
    if !profile.is_valid() {
        return Err(Error(format!(
            "services.workload_routing: node `{}` has an invalid id or an invalid label",
            cfg.id
        )));
    }
    Ok(profile)
}

/// Every node of the topology, including this one.
fn build_topology(settings: &Settings) -> Result<Vec<NodeProfile>> {
    if settings.nodes.is_empty() {
        return Err(Error(
            "services.workload_routing.nodes names no node".to_owned(),
        ));
    }
    settings.nodes.iter().map(node_profile).collect()
}

/// The routing policy: the standard role → queue mapping and the standard report path rule
/// (`spot_workload_routing::policy::RoutingPolicy::standard`), with the replica-read rules built
/// **only** from the allow-listed `replica_reports` — never a blanket rule for every report, so a
/// report not named there is never served from a replica however fresh one is.
fn build_policy(settings: &Settings) -> Result<RoutingPolicy> {
    let standard = RoutingPolicy::standard();
    let mut replica_reads = Vec::new();
    for report in &settings.replica_reports {
        if report.report.is_empty() {
            return Err(Error(
                "services.workload_routing.replica_reports: a report has no name".to_owned(),
            ));
        }
        if report.max_lag_ms == 0 {
            return Err(Error(format!(
                "services.workload_routing.replica_reports: report `{}` sets max_lag_ms to zero",
                report.report
            )));
        }
        replica_reads.push(ReplicaReadRule {
            matcher: PathMatcher::exact(format!("/api/v1/report/{}/run", report.report)),
            max_lag_ms: report.max_lag_ms,
            staleness_tolerant: true,
        });
    }
    let policy = RoutingPolicy {
        queue_affinity: standard.queue_affinity,
        path_rules: standard.path_rules,
        replica_reads,
        max_hops: settings.max_hops.max(1),
        ..RoutingPolicy::default()
    };
    policy
        .validate()
        .map_err(|e| Error(format!("services.workload_routing: {e}")))?;
    Ok(policy)
}

fn build_replicas(settings: &Settings) -> Vec<ReplicaStatus> {
    settings
        .replicas
        .iter()
        .map(|r| ReplicaStatus {
            id: NodeId::new(r.id.clone()),
            applied_token: SessionToken(r.applied_token),
            lag: Duration::from_millis(r.lag_ms),
        })
        .collect()
}

/// The redirect base URL of every node that declared one.
fn build_hosts(settings: &Settings) -> HashMap<String, String> {
    settings
        .nodes
        .iter()
        .filter_map(|n| n.host.clone().map(|host| (n.id.clone(), host)))
        .collect()
}

// ---- applying the decision ----

/// What one request's routing decision becomes.
struct Shared {
    engine: Arc<RoutingEngine>,
    fallback_to_local: bool,
    hosts: HashMap<String, String>,
    local: NodeId,
}

impl QueueAffinityPort for Shared {
    fn queues_for_local(&self) -> Vec<String> {
        self.engine
            .queues_for_local()
            .into_iter()
            .map(QueueName::into_string)
            .collect()
    }

    fn local_node_id(&self) -> String {
        self.local.as_str().to_owned()
    }
}

/// The read preference a local decision carries: the engine already folds a replica choice into
/// [`RouteDecision::Replica`], so this mirrors `spot_workload_routing::middleware`'s own private
/// helper rather than reaching into it.
fn read_preference(outcome: &RouteOutcome) -> ReadPreference {
    match outcome.decision {
        RouteDecision::Replica { max_lag } => ReadPreference::Replica { max_lag },
        _ => outcome.read.preference,
    }
}

/// What the host does with a request, once routed.
#[derive(Debug, Clone, PartialEq)]
enum Decision {
    /// Answer the caller with a redirect to this `Location`.
    Redirect(String),
    /// The target is unavailable and `fallback_to_local` is `false`: refuse the request.
    Refuse,
    /// Serve the request here, carrying this outcome and read preference.
    Continue(RouteOutcome, ReadPreference),
}

/// The pure decision for one request: no I/O, so a test drives it directly.
fn decide(shared: &Shared, method: &Method, path: &str, path_and_query: &str, headers: &HeaderMap) -> Decision {
    let meta = meta_from_parts(method, path, headers, &NoSessions);
    let outcome = shared.engine.decide(&meta);
    if let RouteDecision::Forward(target) = &outcome.decision {
        return match shared.hosts.get(target.as_str()) {
            Some(host) => Decision::Redirect(format!("{host}{path_and_query}")),
            None if shared.fallback_to_local => Decision::Continue(
                RouteOutcome {
                    decision: RouteDecision::Local,
                    // Same catalogued code as the `Refuse` branch below: no host is configured
                    // for the engine's chosen target, so the request is served locally instead.
                    fallback: Some("workload_routing.no_target"),
                    ..outcome
                },
                ReadPreference::Primary,
            ),
            None => Decision::Refuse,
        };
    }
    let read = read_preference(&outcome);
    Decision::Continue(outcome, read)
}

/// The `axum` middleware: redirects a report request routed elsewhere, refuses one whose target
/// is unavailable and fallback is off, and otherwise carries the decision into the request's
/// extensions for the handler (and, eventually, the storage layer) to read.
async fn route_request(State(shared): State<Arc<Shared>>, mut request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path().to_owned();
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| path.clone());
    let headers = request.headers().clone();
    match decide(&shared, &method, &path, &path_and_query, &headers) {
        Decision::Redirect(location) => {
            let mut response = StatusCode::TEMPORARY_REDIRECT.into_response();
            if let Ok(value) = HeaderValue::from_str(&location) {
                response.headers_mut().insert(header::LOCATION, value);
            }
            response
        }
        Decision::Refuse => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":{"code":"workload_routing.no_target"}}"#,
        )
            .into_response(),
        Decision::Continue(outcome, read) => {
            request.extensions_mut().insert(outcome);
            request.extensions_mut().insert(read);
            next.run(request).await
        }
    }
}

// ---- the hosted service ----

struct RoutingService {
    shared: Arc<Shared>,
}

impl Service for RoutingService {
    fn name(&self) -> &'static str {
        "workload_routing"
    }

    fn level(&self) -> Level {
        // The feature is advisory and additive: a misbehaving policy degrades routing (every
        // request falls back to local, which is exactly the pre-T1 behaviour), never the site.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        let node = self.shared.local_node_id();
        Box::pin(async move {
            tracing::debug!(node, "workload routing is live");
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The sites built so far, by site name, so a reload finds the same engine rather than building
/// a second one (the exact pattern `report.rs` uses for its own port).
fn built() -> &'static Mutex<HashMap<String, Arc<Shared>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<Shared>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// Registers workload routing: the policy engine over `[services.workload_routing]`, the
/// [`QueueAffinityPort`] every later module may read, and the router layer that applies report
/// routing and replica-read decisions to every request.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("workload_routing")?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.label().to_owned();
    let shared = if ctx.assembling() {
        let nodes = build_topology(&settings)?;
        if !nodes.iter().any(|n| n.id.as_str() == settings.local_node) {
            return Err(Error(format!(
                "services.workload_routing.local_node `{}` does not name a node in \
                 services.workload_routing.nodes",
                settings.local_node
            )));
        }
        let policy = build_policy(&settings)?;
        let replicas = build_replicas(&settings);
        let hosts = build_hosts(&settings);
        let local = NodeId::new(settings.local_node.clone());
        let clock = Arc::clone(ctx.clock());
        let engine = Arc::new(RoutingEngine::new(
            Arc::new(PolicyStore::new(policy)),
            nodes,
            replicas,
            local.clone(),
            Arc::new(NoMetrics) as Arc<dyn Metrics>,
            Arc::new(move || clock.now_utc()),
        ));
        let shared = Arc::new(Shared {
            engine,
            fallback_to_local: settings.fallback_to_local,
            hosts,
            local,
        });
        ctx.add(Arc::new(RoutingService {
            shared: Arc::clone(&shared),
        }));
        lock(built()).insert(site.clone(), Arc::clone(&shared));
        shared
    } else {
        match lock(built()).get(&site).cloned() {
            Some(shared) => shared,
            None => return Ok(()),
        }
    };
    ctx.provide::<dyn QueueAffinityPort>(Arc::clone(&shared) as Arc<dyn QueueAffinityPort>)?;
    ctx.router_layer(move |base| base.layer(axum::middleware::from_fn_with_state(shared, route_request)));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn settings_two_roles() -> Settings {
        Settings {
            local_node: "a".to_owned(),
            nodes: vec![
                NodeSettings {
                    id: "a".to_owned(),
                    roles: vec!["interactive".to_owned()],
                    host: Some("https://a.internal".to_owned()),
                    ..NodeSettings::default()
                },
                NodeSettings {
                    id: "c".to_owned(),
                    roles: vec!["report".to_owned()],
                    host: Some("https://c.internal".to_owned()),
                    ..NodeSettings::default()
                },
            ],
            ..Settings::default()
        }
    }

    fn shared_of(settings: &Settings) -> Shared {
        let nodes = build_topology(settings).expect("topology");
        let policy = build_policy(settings).expect("policy");
        let replicas = build_replicas(settings);
        let hosts = build_hosts(settings);
        let local = NodeId::new(settings.local_node.clone());
        let at = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let engine = Arc::new(RoutingEngine::new(
            Arc::new(PolicyStore::new(policy)),
            nodes,
            replicas,
            local.clone(),
            Arc::new(NoMetrics) as Arc<dyn Metrics>,
            Arc::new(move || at),
        ));
        Shared {
            engine,
            fallback_to_local: settings.fallback_to_local,
            hosts,
            local,
        }
    }

    #[test]
    fn two_node_roles_make_queue_affinity_visible() {
        let settings = settings_two_roles();
        let interactive = shared_of(&settings);
        assert_eq!(interactive.queues_for_local(), vec!["default", "interactive"]);

        let mut report_settings = settings_two_roles();
        report_settings.local_node = "c".to_owned();
        let report = shared_of(&report_settings);
        assert_eq!(report.queues_for_local(), vec!["reports", "exports"]);
    }

    #[test]
    fn a_report_request_on_the_interactive_node_redirects_to_the_report_node() {
        let shared = shared_of(&settings_two_roles());
        let decision = decide(
            &shared,
            &Method::GET,
            "/api/v1/report/sales/run",
            "/api/v1/report/sales/run",
            &HeaderMap::new(),
        );
        assert_eq!(
            decision,
            Decision::Redirect("https://c.internal/api/v1/report/sales/run".to_owned())
        );
    }

    #[test]
    fn the_report_node_serves_its_own_report_requests_locally() {
        let mut settings = settings_two_roles();
        settings.local_node = "c".to_owned();
        let shared = shared_of(&settings);
        let decision = decide(
            &shared,
            &Method::GET,
            "/api/v1/report/sales/run",
            "/api/v1/report/sales/run",
            &HeaderMap::new(),
        );
        match decision {
            Decision::Continue(outcome, read) => {
                assert!(outcome.decision.is_local());
                assert_eq!(read, ReadPreference::Primary);
            }
            other => panic!("expected a local decision, got {other:?}"),
        }
    }

    #[test]
    fn an_unavailable_target_falls_back_to_local_only_when_the_setting_allows_it() {
        // No node carries the `report` role at all: the pool is empty, which the crate itself
        // always folds back to local (this is not the setting's concern).
        let mut settings = settings_two_roles();
        settings.nodes.retain(|n| n.id != "c");
        let shared = shared_of(&settings);
        let decision = decide(
            &shared,
            &Method::GET,
            "/api/v1/report/sales/run",
            "/api/v1/report/sales/run",
            &HeaderMap::new(),
        );
        assert!(matches!(decision, Decision::Continue(_, _)));

        // A report node exists but declares no `host`: this module's own "unavailable" case,
        // which the setting does control.
        let mut settings = settings_two_roles();
        settings.nodes[1].host = None;
        settings.fallback_to_local = true;
        let shared = shared_of(&settings);
        let decision = decide(
            &shared,
            &Method::GET,
            "/api/v1/report/sales/run",
            "/api/v1/report/sales/run",
            &HeaderMap::new(),
        );
        assert!(matches!(decision, Decision::Continue(_, _)), "{decision:?}");

        settings.fallback_to_local = false;
        let shared = shared_of(&settings);
        let decision = decide(
            &shared,
            &Method::GET,
            "/api/v1/report/sales/run",
            "/api/v1/report/sales/run",
            &HeaderMap::new(),
        );
        assert_eq!(decision, Decision::Refuse);
    }

    #[test]
    fn only_allow_listed_reports_may_use_a_lagging_or_fresh_replica() {
        let mut settings = settings_two_roles();
        settings.local_node = "c".to_owned();
        settings.replicas = vec![ReplicaSettings {
            id: "r1".to_owned(),
            lag_ms: 50,
            applied_token: 0,
        }];
        settings.replica_reports = vec![ReplicaReportSettings {
            report: "sales".to_owned(),
            max_lag_ms: 5_000,
        }];
        let shared = shared_of(&settings);

        // Allow-listed, replica within the bound: the read may use it.
        let decision = decide(
            &shared,
            &Method::GET,
            "/api/v1/report/sales/run",
            "/api/v1/report/sales/run",
            &HeaderMap::new(),
        );
        match decision {
            Decision::Continue(_, read) => assert!(read.is_replica(), "{read:?}"),
            other => panic!("expected a local decision, got {other:?}"),
        }

        // Not allow-listed: the primary, always, however fresh the replica is.
        let decision = decide(
            &shared,
            &Method::GET,
            "/api/v1/report/other/run",
            "/api/v1/report/other/run",
            &HeaderMap::new(),
        );
        match decision {
            Decision::Continue(_, read) => assert_eq!(read, ReadPreference::Primary),
            other => panic!("expected a local decision, got {other:?}"),
        }
    }

    #[test]
    fn a_lagging_replica_falls_back_to_the_primary() {
        let mut settings = settings_two_roles();
        settings.local_node = "c".to_owned();
        // The replica's lag is well past the report's own bound: it fails the staleness test.
        settings.replicas = vec![ReplicaSettings {
            id: "r1".to_owned(),
            lag_ms: 9_000,
            applied_token: 0,
        }];
        settings.replica_reports = vec![ReplicaReportSettings {
            report: "sales".to_owned(),
            max_lag_ms: 5_000,
        }];
        let shared = shared_of(&settings);
        let decision = decide(
            &shared,
            &Method::GET,
            "/api/v1/report/sales/run",
            "/api/v1/report/sales/run",
            &HeaderMap::new(),
        );
        match decision {
            Decision::Continue(_, read) => assert_eq!(read, ReadPreference::Primary),
            other => panic!("expected a local decision, got {other:?}"),
        }
    }

    #[test]
    fn a_write_to_a_replica_eligible_report_still_reads_the_primary() {
        let mut settings = settings_two_roles();
        settings.local_node = "c".to_owned();
        settings.replicas = vec![ReplicaSettings {
            id: "r1".to_owned(),
            lag_ms: 10,
            applied_token: 0,
        }];
        settings.replica_reports = vec![ReplicaReportSettings {
            report: "sales".to_owned(),
            max_lag_ms: 5_000,
        }];
        let shared = shared_of(&settings);
        let decision = decide(
            &shared,
            &Method::POST,
            "/api/v1/report/sales/run",
            "/api/v1/report/sales/run",
            &HeaderMap::new(),
        );
        match decision {
            Decision::Continue(_, read) => assert_eq!(read, ReadPreference::Primary),
            other => panic!("expected a local decision, got {other:?}"),
        }
    }

    #[test]
    fn settings_parse_from_toml() {
        let toml = r#"
            local_node = "a"
            fallback_to_local = false

            [[nodes]]
            id = "a"
            roles = ["interactive"]

            [[nodes]]
            id = "c"
            roles = ["report"]
            host = "https://c.internal"

            [[replica_reports]]
            report = "sales"
            max_lag_ms = 2000
        "#;
        let settings: Settings = toml::from_str(toml).expect("parses");
        assert_eq!(settings.local_node, "a");
        assert!(!settings.fallback_to_local);
        assert_eq!(settings.nodes.len(), 2);
        assert_eq!(settings.replica_reports[0].max_lag_ms, 2000);
    }

    #[test]
    fn an_unknown_role_is_a_typed_error() {
        let mut settings = settings_two_roles();
        settings.nodes[0].roles = vec!["made_up".to_owned()];
        let err = build_topology(&settings).expect_err("unknown role");
        assert!(err.0.contains("made_up"), "{err}");
    }

    #[test]
    fn a_local_node_id_absent_from_the_topology_is_refused_at_registration() {
        let settings = Settings {
            local_node: "missing".to_owned(),
            ..settings_two_roles()
        };
        let nodes = build_topology(&settings).expect("topology");
        assert!(!nodes.iter().any(|n| n.id.as_str() == settings.local_node));
    }

    #[test]
    fn an_empty_report_name_in_the_allow_list_is_refused() {
        let mut settings = settings_two_roles();
        settings.replica_reports = vec![ReplicaReportSettings {
            report: String::new(),
            max_lag_ms: 1000,
        }];
        let err = build_policy(&settings).expect_err("empty report name");
        assert!(err.0.contains("has no name"), "{err}");
    }
}
