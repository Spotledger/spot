//! `workload_routing.rs` hosted (WP-PLT-20.T2): queue affinity is visible through the port, a
//! report request routes to the dedicated node (or runs locally there), an unavailable target
//! falls back or fails exactly per `fallback_to_local`, and the staleness bound a replica-eligible
//! report declares is enforced with a fake lag. None of this needs PostgreSQL: the policy engine
//! and this hosting module are pure decisions over settings and a topology, so every case runs
//! without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::{Request, StatusCode};
use axum::response::Json;
use axum::routing::get;
use serde_json::{Value as Json2, json};
use spot_cli::services::{SiteCtx, workload_routing};
use spot_core::ManualClock;
use spot_http::api::ports::routing::QueueAffinityPort;
use spot_workload_routing::read::ReadPreference;
use spot_workload_routing::route::RouteOutcome;
use time::macros::datetime;
use tower::ServiceExt;

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-08 10:00:00 UTC)))
}

fn settings_map(inner: &str) -> BTreeMap<String, toml::Value> {
    let value: toml::Value = toml::from_str(inner).expect("settings parse");
    let mut map = BTreeMap::new();
    map.insert("workload_routing".to_owned(), value);
    map
}

fn ctx(inner: &str) -> SiteCtx<'static> {
    SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        settings_map(inner),
        true,
    )
}

/// Echoes the request's routing decision as JSON, so a test can see exactly what the middleware
/// attached without needing a real report handler.
async fn echo(
    Extension(outcome): Extension<RouteOutcome>,
    Extension(read): Extension<ReadPreference>,
) -> Json<Json2> {
    Json(json!({
        "local": outcome.decision.is_local(),
        "replica": read.is_replica(),
    }))
}

fn router(ctx: SiteCtx<'static>) -> Router {
    let base = Router::new().route("/api/v1/report/{report}/run", get(echo));
    ctx.finish(base).router
}

async fn get_request(router: Router, path: &str) -> axum::http::Response<Body> {
    router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

const TWO_NODES: &str = r#"
local_node = "a"

[[nodes]]
id = "a"
roles = ["interactive"]

[[nodes]]
id = "c"
roles = ["report"]
host = "https://c.internal"
"#;

#[test]
fn queue_affinity_is_visible_per_role() {
    let mut site = ctx(TWO_NODES);
    workload_routing::register(&mut site).unwrap();
    let port = site.port::<dyn QueueAffinityPort>().expect("port provided");
    assert_eq!(port.queues_for_local(), vec!["default", "interactive"]);
    assert_eq!(port.local_node_id(), "a");

    let report_nodes = TWO_NODES.replace("local_node = \"a\"", "local_node = \"c\"");
    let mut site = ctx(&report_nodes);
    workload_routing::register(&mut site).unwrap();
    let port = site.port::<dyn QueueAffinityPort>().expect("port provided");
    assert_eq!(port.queues_for_local(), vec!["reports", "exports"]);
}

#[tokio::test]
async fn a_report_request_on_the_interactive_node_is_redirected_to_the_report_node() {
    let mut site = ctx(TWO_NODES);
    workload_routing::register(&mut site).unwrap();
    let response = get_request(router(site), "/api/v1/report/sales/run").await;
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "https://c.internal/api/v1/report/sales/run"
    );
}

#[tokio::test]
async fn the_report_node_serves_its_own_report_requests_locally() {
    let report_nodes = TWO_NODES.replace("local_node = \"a\"", "local_node = \"c\"");
    let mut site = ctx(&report_nodes);
    workload_routing::register(&mut site).unwrap();
    let response = get_request(router(site), "/api/v1/report/sales/run").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Json2 = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["local"], json!(true));
    assert_eq!(json["replica"], json!(false));
}

/// Node `c` carries the `report` role but declares no `host`: an unavailable redirect target.
/// `{fallback}` must come before any `[[nodes]]` table, or TOML parses it as belonging to the
/// last node instead of the top-level settings.
const NO_HOST_REPORT_NODE: &str = r#"
local_node = "a"
fallback_to_local = {fallback}

[[nodes]]
id = "a"
roles = ["interactive"]

[[nodes]]
id = "c"
roles = ["report"]
"#;

#[tokio::test]
async fn an_unavailable_target_falls_back_to_local_only_when_the_setting_allows_it() {
    let allowed = NO_HOST_REPORT_NODE.replace("{fallback}", "true");
    let mut site = ctx(&allowed);
    workload_routing::register(&mut site).unwrap();
    let response = get_request(router(site), "/api/v1/report/sales/run").await;
    assert_eq!(response.status(), StatusCode::OK, "falls back to local");

    let refused = NO_HOST_REPORT_NODE.replace("{fallback}", "false");
    let mut site = ctx(&refused);
    workload_routing::register(&mut site).unwrap();
    let response = get_request(router(site), "/api/v1/report/sales/run").await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "refused per the setting"
    );
}

const REPLICA_SETTINGS: &str = r#"
local_node = "c"

[[nodes]]
id = "a"
roles = ["interactive"]

[[nodes]]
id = "c"
roles = ["report"]
host = "https://c.internal"

[[replicas]]
id = "r1"
lag_ms = 50

[[replica_reports]]
report = "sales"
max_lag_ms = 5000
"#;

#[tokio::test]
async fn an_allow_listed_report_within_the_staleness_bound_reads_the_replica() {
    let mut site = ctx(REPLICA_SETTINGS);
    workload_routing::register(&mut site).unwrap();
    let response = get_request(router(site), "/api/v1/report/sales/run").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Json2 = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["replica"], json!(true));
}

#[tokio::test]
async fn a_lagging_replica_falls_back_to_the_primary_with_a_fake_lag() {
    // The replica's lag (9000ms) is well past the report's own bound (5000ms).
    let lagging = REPLICA_SETTINGS.replace("\nlag_ms = 50\n", "\nlag_ms = 9000\n");
    let mut site = ctx(&lagging);
    workload_routing::register(&mut site).unwrap();
    let response = get_request(router(site), "/api/v1/report/sales/run").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Json2 = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["replica"], json!(false), "falls back to the primary");
}

#[tokio::test]
async fn a_report_not_on_the_allow_list_never_reads_the_replica() {
    let mut site = ctx(REPLICA_SETTINGS);
    workload_routing::register(&mut site).unwrap();
    let response = get_request(router(site), "/api/v1/report/other/run").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Json2 = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["replica"], json!(false));
}

#[test]
fn a_local_node_id_absent_from_the_topology_is_refused_at_registration() {
    let bad = TWO_NODES.replace("local_node = \"a\"", "local_node = \"missing\"");
    let mut site = ctx(&bad);
    let err = workload_routing::register(&mut site).unwrap_err();
    assert!(err.0.contains("missing"), "{err}");
}

#[test]
fn disabled_routing_registers_no_port_and_no_router_edit() {
    let mut site = ctx("enabled = false\n");
    workload_routing::register(&mut site).unwrap();
    assert!(site.port::<dyn QueueAffinityPort>().is_none());
}

#[test]
fn an_empty_settings_table_runs_as_one_node_and_still_registers() {
    let mut site = ctx("");
    workload_routing::register(&mut site).unwrap();
    let port = site.port::<dyn QueueAffinityPort>().expect("port provided");
    assert_eq!(port.local_node_id(), "local");
    assert_eq!(port.queues_for_local(), vec!["default", "interactive"]);
}
