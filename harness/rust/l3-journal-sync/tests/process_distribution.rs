//! Process distribution end to end on two real nodes (WP-SYN-12.T2): a process published at the
//! hub reaches an edge, a Case started on the edge runs on its pinned version, and an edge one
//! release behind keeps running.
//!
//! # What is proved today, and what waits
//!
//! The rules of the distribution (publish with scope keys, install beside, hold, release on
//! upgrade, retire only unpinned, refuse an edge-authored `process:*`, durable before answering)
//! are proved in `crates/framework/spot-cli/tests/sync_process_host.rs` against the hosted
//! `ProcessSync` and in `spot-sync-process`'s own suite. The scenarios here need three things
//! that had not landed: the hub's hosted sync listener (WP-RTB-02.T4 to T8), the node role
//! configuration (WP-RTB-03.T1) and the BPM runtime's HTTP start of a Case (WP-BPM-02.T4). Until
//! they do, every scenario probes for them and **prints `SKIPPED:`** (never a mock of the thing
//! under test); the `#[ignore]`d ones also need a harness helper named in their reason.
//!
//! The scenarios, from the brief: (a) publish v1 at the hub, the edge has it within one sync cycle
//! and starts a Case on v1; (b) publish v2, the edge installs it beside v1, the running Case
//! finishes on v1, a new Case starts on v2; (c) the edge is one release behind (engine version
//! below `min_engine_version` of v3): v3 is held, new Cases keep starting on v2, the hold shows in
//! the sync health, an upgrade releases it; (d) an out-of-scope edge never receives the record;
//! (e) an edge-authored `process:*` record pushed by a hostile client is refused and audited.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_journal_sync::{Client, Cluster, Node, NodeSpec, Role};

fn spec(name: &str, role: Role) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role,
        extra_config_toml: String::new(),
    }
}

async fn serves(node: &Node, path: &str) -> bool {
    let Some(client) = node.client() else {
        return false;
    };
    let anonymous = Client::new(client.addr(), node.site());
    matches!(
        anonymous.send("GET", path, None).await,
        Ok(reply) if reply.status != 404 && reply.status != 501
    )
}

/// A hub and an edge, or `None` (with the reason printed) when PostgreSQL, hosted sync or the
/// process API is missing.
async fn hub_and_edge(extra_edge: bool) -> Option<Cluster> {
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("hub", Role::Hub)).await.is_none() {
        println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
        return None;
    }
    cluster
        .add_node(spec(
            "edge",
            Role::Edge {
                hub: "hub".to_owned(),
            },
        ))
        .await?;
    if extra_edge {
        cluster
            .add_node(spec(
                "edge-far",
                Role::Edge {
                    hub: "hub".to_owned(),
                },
            ))
            .await?;
    }
    if !serves(cluster.node("hub"), "/sync/v1/enrol").await {
        println!("SKIPPED: the hub does not serve sync yet (WP-RTB-02.T4 to T8 hosting)");
        return None;
    }
    if !serves(cluster.node("edge"), "/api/v1/process").await {
        println!("SKIPPED: the process API is not served yet (WP-BPM-02.T4 hosting)");
        return None;
    }
    Some(cluster)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs the hosted hub, node roles (WP-RTB-03.T1), the process publish and start-a-Case HTTP API, and an enrolment helper in the harness"]
async fn a_published_process_reaches_the_edge_and_a_case_starts_on_its_version() {
    let Some(_cluster) = hub_and_edge(false).await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs the same plus a Case that stays open while v2 is published"]
async fn a_new_version_installs_beside_and_a_running_case_finishes_on_the_old_one() {
    let Some(_cluster) = hub_and_edge(false).await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs a settable engine version per node (services.sync_process.engine_version) in the harness"]
async fn an_edge_one_release_behind_holds_the_new_version_and_an_upgrade_releases_it() {
    let Some(_cluster) = hub_and_edge(false).await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs subscription scope configuration in the harness: an edge outside the process scope never receives it"]
async fn an_out_of_scope_edge_never_receives_the_process() {
    let Some(_cluster) = hub_and_edge(true).await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs an enrolled node client to push a hostile process:* record at the hub"]
async fn an_edge_authored_process_record_is_refused_and_audited() {
    let Some(_cluster) = hub_and_edge(false).await else {
        return;
    };
}
