//! The two-node sync end-to-end suite on real hosts (WP-RTB-02.T9): one hub and two edges, each a
//! real `spot serve` assembly with its own database, over the harness's loopback network.
//!
//! # What runs today
//!
//! The hosted sync listener (WP-RTB-02.T4 to T8) had not landed when this file was written, so
//! the harness has no sync endpoint to talk to. Every scenario therefore starts with
//! [`topology`], which brings the three nodes up and **probes** whether the hub serves sync; when
//! it does not, the test prints `SKIPPED:` and returns (it never fakes a pass with a mock of the
//! thing under test). Once a hub serves `/sync/v1/*` the scenarios below run unchanged.
//!
//! # Scenarios (the brief's list)
//!
//! (a) master data hub to both edges; (b) record data edge to hub; (c) scope: edge B never gets
//! edge A's records; (f) restart of hub and of an edge mid-stream; (g) an edge offline while 1,000
//! changes accumulate catches up in order; (i) acknowledgement by serve sequence under out-of-order
//! serving (the WP-SYN-11.T2 regression). Blocked and `#[ignore]`d with their reason: (d) closure
//! (needs a linked-documents fixture and `CLOSURE_TOO_LARGE`), (e) ownership and branch series
//! (needs WP-RTB-03.T2, owner: the edge-offline lane), (h) a runtime-created DocType (needs the
//! customise fixture of the harness).
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention). A failing
//! run keeps its databases with `L3_KEEP=1` (see the project process notes).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use l3_journal_sync::{Client, Cluster, Node, NodeSpec, Role, seed, wait_until};
use spot_postgres::{PgBackend, PgConfig};

const SETTLE: Duration = Duration::from_secs(60);

fn spec(name: &str, role: Role) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role,
        extra_config_toml: String::new(),
    }
}

/// Whether a hub serves the sync endpoints: the enrolment route answers something other than "no
/// such route" (404) or "not implemented" (501) to a request without a code.
async fn serves_sync(hub: &Node) -> bool {
    if !l3_journal_sync::hub_is_wired(hub) {
        return false;
    }
    let Some(client) = hub.client() else {
        return false;
    };
    let anonymous = Client::new(client.addr(), hub.site());
    match anonymous
        .send("POST", "/sync/v1/enrol", Some(&serde_json::json!({})))
        .await
    {
        Ok(reply) => reply.status != 404 && reply.status != 501,
        Err(_) => false,
    }
}

/// Hub, edge A and edge B; `None` (with the reason printed) when PostgreSQL or hosted sync is
/// missing.
async fn topology() -> Option<Cluster> {
    let mut cluster = Cluster::new();
    cluster.add_node(spec("hub", Role::Hub)).await?;
    for edge in ["edge-a", "edge-b"] {
        cluster
            .add_node(spec(
                edge,
                Role::Edge {
                    hub: "hub".to_owned(),
                },
            ))
            .await?;
    }
    if !serves_sync(cluster.node("hub")).await {
        println!("SKIPPED: the hub does not serve sync yet (WP-RTB-02.T4 to T8 hosting)");
        return None;
    }
    Some(cluster)
}

#[allow(dead_code)]
fn no_postgres() {
    println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
}

/// The ids of every `project` document on `node`, sorted.
async fn project_ids(node: &Node) -> Vec<String> {
    let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
    let config = PgConfig::from_url(&url)
        .unwrap()
        .with_dbname(node.db_name());
    let backend = PgBackend::connect_unchecked(config).unwrap();
    let client = backend.pool().get().await.unwrap();
    client
        .query("SELECT id::text FROM project ORDER BY id", &[])
        .await
        .map(|rows| rows.iter().map(|r| r.get::<_, String>(0)).collect())
        .unwrap_or_default()
}

/// Waits until `node` holds every id in `wanted`.
async fn has_all(node: &Node, wanted: &[String]) -> Result<(), String> {
    wait_until(SETTLE, || async {
        let held = project_ids(node).await;
        wanted.iter().all(|id| held.contains(id))
    })
    .await
    .map_err(|e| format!("{}: {e}", node.name()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_change_reaches_both_edges_and_a_later_change_reaches_them_consistently() {
    let Some(cluster) = topology().await else {
        return;
    };
    let ids = seed::documents(cluster.node("hub"), 5).await.unwrap();
    for edge in ["edge-a", "edge-b"] {
        has_all(cluster.node(edge), &ids).await.unwrap();
    }
    let more = seed::documents(cluster.node("hub"), 1).await.unwrap();
    for edge in ["edge-a", "edge-b"] {
        has_all(cluster.node(edge), &more).await.unwrap();
    }
    assert_eq!(
        project_ids(cluster.node("edge-a")).await,
        project_ids(cluster.node("edge-b")).await,
        "both edges hold the same master data"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_record_reaches_the_hub() {
    let Some(cluster) = topology().await else {
        return;
    };
    let ids = seed::documents(cluster.node("edge-a"), 3).await.unwrap();
    has_all(cluster.node("hub"), &ids).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edge_b_never_receives_edge_a_records() {
    let Some(cluster) = topology().await else {
        return;
    };
    let ids = seed::documents(cluster.node("edge-a"), 4).await.unwrap();
    has_all(cluster.node("hub"), &ids).await.unwrap();
    // Give the hub every chance to (wrongly) forward them, then look.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let held = project_ids(cluster.node("edge-b")).await;
    assert!(
        ids.iter().all(|id| !held.contains(id)),
        "scope keys keep edge A's records off edge B (ADR-063)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_of_the_hub_and_of_an_edge_mid_stream_loses_and_duplicates_nothing() {
    let Some(mut cluster) = topology().await else {
        return;
    };
    let first = seed::documents(cluster.node("hub"), 20).await.unwrap();
    cluster.node_mut("edge-a").stop().await;
    let second = seed::documents(cluster.node("hub"), 20).await.unwrap();
    cluster.node_mut("hub").stop().await;
    cluster.node_mut("hub").start().await;
    cluster.node_mut("edge-a").start().await;
    let all: Vec<String> = first.into_iter().chain(second).collect();
    has_all(cluster.node("edge-a"), &all).await.unwrap();
    has_all(cluster.node("edge-b"), &all).await.unwrap();
    let held = project_ids(cluster.node("edge-a")).await;
    let mut unique = held.clone();
    unique.dedup();
    assert_eq!(held, unique, "no duplicate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_offline_while_a_thousand_changes_accumulate_catches_up() {
    let Some(mut cluster) = topology().await else {
        return;
    };
    cluster.node_mut("edge-a").stop().await;
    let ids = seed::documents(cluster.node("hub"), 1000).await.unwrap();
    cluster.node_mut("edge-a").start().await;
    has_all(cluster.node("edge-a"), &ids).await.unwrap();
    let held = project_ids(cluster.node("edge-a")).await;
    let mut unique = held.clone();
    unique.dedup();
    assert_eq!(held, unique, "no duplicate after catching up");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledgement_is_by_serve_sequence_under_out_of_order_serving() {
    // The WP-SYN-11.T2 regression, end to end: pause edge A's link mid-stream, let the hub serve
    // more, heal, and nothing is skipped or applied twice.
    let Some(cluster) = topology().await else {
        return;
    };
    let hub = cluster.node("hub");
    let edge = cluster.node("edge-a");
    let first = seed::documents(hub, 10).await.unwrap();
    has_all(edge, &first).await.unwrap();
    hub.partition_from(edge).await;
    let during = seed::documents(hub, 30).await.unwrap();
    hub.heal(edge);
    has_all(edge, &during).await.unwrap();
    let held = project_ids(edge).await;
    let mut unique = held.clone();
    unique.dedup();
    assert_eq!(held, unique);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "blocked: needs a linked-documents fixture and the CLOSURE_TOO_LARGE condition"]
async fn a_closure_over_the_limit_sends_nothing_partial() {
    let Some(_cluster) = topology().await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "blocked on WP-RTB-03.T2 (per-branch naming series); owner: the edge-offline lane"]
async fn a_second_writer_for_a_partition_is_rejected_and_branch_series_do_not_collide() {
    let Some(_cluster) = topology().await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "blocked: needs the harness's customise fixture to create a DocType at runtime"]
async fn a_runtime_created_doctype_syncs() {
    let Some(_cluster) = topology().await else {
        return;
    };
}
