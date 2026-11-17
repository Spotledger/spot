//! A branch runs a full simulated trading day offline (WP-RTB-03.T4): one hub, three edges, a
//! second tenant on its own hub and an unenrolled edge, with document traffic, a cut of the
//! hub link for all three edges, a mid-day restart, an edge brought down and back, and an
//! overnight return in staggered order. Tests only: a defect found is a follow-up task.
//!
//! # What runs today
//!
//! The hosted sync listener (WP-RTB-02.T4 to T8), the leased naming series (WP-RTB-03.T2) and the
//! offline identity lease (WP-RTB-03.T3) had not landed when this was written, so the test probes
//! for the hub's sync endpoint and **prints `SKIPPED:`** until it is served (never a mock of the
//! thing under test). Once it is served the day below runs unchanged. The assertions that need
//! those services' own observable state are listed as pending in the project process notes
//! rather than faked here.
//!
//! # The day
//!
//! morning sync; the hub's master data changes and every edge converges; the hub link is cut for
//! all three edges; each edge creates documents all day (quick profile 300 each, full profile
//! 5,000 with `L3_CHAOS=1`); edge B restarts mid-day (kill and start); edge C is shut down and
//! brought back; the hub keeps taking its own writes; overnight the cut heals in staggered order
//! (A, then B and C).
//!
//! # Assertions after convergence (queries, not logs)
//!
//! (a) every edge document exists at the hub, and the hub's own writes at every edge;
//! (b) no duplicate document name across all nodes; (c) no lost write: the count each edge
//! acknowledged equals the hub's count of that edge's records; (d) the second tenant's documents
//! never appear on any edge of this tenant and the unenrolled edge holds nothing; (e) the edge
//! stayed ready during the cut. Pending (needs the leased series, the identity lease and the
//! conditions view): `SERIES_LOW`, `NODE_SILENT`, identity-lease lapse, handover adoption order,
//! ownership of every record, the `Calm` health view, fingerprints of the shared scope and the
//! per-node journal chain.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::time::Duration;

use l3_journal_sync::{Client, Cluster, Node, NodeSpec, Role, wait_until};
use spot_postgres::{PgBackend, PgConfig};

const SETTLE: Duration = Duration::from_secs(180);
const EDGES: [&str; 3] = ["edge-a", "edge-b", "edge-c"];

fn spec(name: &str, role: Role) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role,
        extra_config_toml: String::new(),
    }
}

async fn serves_sync(hub: &Node) -> bool {
    if !l3_journal_sync::hub_is_wired(hub) {
        return false;
    }
    let Some(client) = hub.client() else {
        return false;
    };
    let anonymous = Client::new(client.addr(), hub.site());
    matches!(
        anonymous.send("POST", "/sync/v1/enrol", Some(&serde_json::json!({}))).await,
        Ok(reply) if reply.status != 404 && reply.status != 501
    )
}

/// The whole setting, or `None` with the reason printed.
async fn setting() -> Option<Cluster> {
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("hub", Role::Hub)).await.is_none() {
        println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
        return None;
    }
    for edge in EDGES {
        cluster
            .add_node(spec(
                edge,
                Role::Edge {
                    hub: "hub".to_owned(),
                },
            ))
            .await?;
    }
    // A second tenant on its own hub, and an edge that is never enrolled.
    cluster.add_node(spec("other-hub", Role::Hub)).await?;
    cluster
        .add_node(spec(
            "stranger",
            Role::Edge {
                hub: "hub".to_owned(),
            },
        ))
        .await?;
    if !serves_sync(cluster.node("hub")).await {
        println!(
            "SKIPPED: the hub does not serve sync yet (WP-RTB-02.T4 to T8, RTB-03.T2/T3 hosting)"
        );
        return None;
    }
    Some(cluster)
}

/// The `code` (the document's own unique name here) of every `project` document on `node`.
async fn names(node: &Node) -> Vec<String> {
    let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
    let config = PgConfig::from_url(&url)
        .unwrap()
        .with_dbname(node.db_name());
    let backend = PgBackend::connect_unchecked(config).unwrap();
    let client = backend.pool().get().await.unwrap();
    client
        .query("SELECT code FROM project ORDER BY code", &[])
        .await
        .map(|rows| rows.iter().map(|r| r.get::<_, String>(0)).collect())
        .unwrap_or_default()
}

async fn has_all(node: &Node, wanted: &BTreeSet<String>) -> Result<(), String> {
    wait_until(SETTLE, || async {
        let held: BTreeSet<String> = names(node).await.into_iter().collect();
        wanted.is_subset(&held)
    })
    .await
    .map_err(|e| format!("{}: {e}", node.name()))
}

/// Creates `n` `project` documents on `node` through its API, as its Administrator, and returns
/// the codes the API acknowledged. `tag` keeps the codes of one batch apart from another's, so a
/// node can write several batches in a day.
async fn write(node: &Node, tag: &str, n: usize) -> Vec<String> {
    let client = node.client().expect("the node is running");
    let owner = node.admin_id();
    let mut acknowledged = Vec::with_capacity(n);
    for i in 0..n {
        let code = format!("{}-{tag}-{i}", node.name());
        let body = serde_json::json!({
            "code": code,
            "title": format!("offline day {code}"),
            "status": "Active",
            "owner": owner,
            "currency": "USD",
            "budget": "0.00",
        });
        let reply = client.create("project", &body).await.unwrap();
        reply.expect(201).unwrap();
        acknowledged.push(code);
    }
    acknowledged
}

async fn ready_during_the_cut(cluster: &Cluster) {
    for edge in EDGES {
        let client = cluster.node(edge).client().expect("the edge keeps running");
        assert_eq!(
            client.health("ready").await.unwrap().status,
            200,
            "{edge} stays ready while the hub is unreachable"
        );
    }
}

async fn day(per_edge: usize) {
    let Some(mut cluster) = setting().await else {
        return;
    };
    // Morning: the hub writes its master data and every edge converges.
    let morning: BTreeSet<String> = write(cluster.node("hub"), "morning", 10)
        .await
        .into_iter()
        .collect();
    for edge in EDGES {
        has_all(cluster.node(edge), &morning).await.unwrap();
    }

    // The hub link is cut for all three edges.
    for edge in EDGES {
        cluster.node(edge).partition_from(cluster.node("hub")).await;
    }

    // The day: each edge works offline; the hub keeps taking its own writes.
    let mut acknowledged: Vec<(String, BTreeSet<String>)> = Vec::new();
    for edge in EDGES {
        let first: BTreeSet<String> = write(cluster.node(edge), "am", per_edge / 2)
            .await
            .into_iter()
            .collect();
        acknowledged.push((edge.to_owned(), first));
    }
    ready_during_the_cut(&cluster).await;

    // Mid-day: edge B is killed and started; edge C is shut down and brought back.
    cluster.node_mut("edge-b").kill();
    // `kill` does not wait: give the host time to be gone before it is started again.
    tokio::time::sleep(Duration::from_secs(3)).await;
    cluster.node_mut("edge-b").start().await;
    cluster.node_mut("edge-c").stop().await;
    cluster.node_mut("edge-c").start().await;
    // The restarted edges still hold the partition (it is on the proxy pair, not in the node).
    for (edge, set) in &mut acknowledged {
        let more: BTreeSet<String> = write(cluster.node(edge), "pm", per_edge - per_edge / 2)
            .await
            .into_iter()
            .collect();
        set.extend(more);
    }
    let hub_day: BTreeSet<String> = write(cluster.node("hub"), "day", 20)
        .await
        .into_iter()
        .collect();
    ready_during_the_cut(&cluster).await;

    // Overnight: the cut heals in staggered order, A first, then B and C.
    cluster.node("edge-a").heal(cluster.node("hub"));
    has_all(cluster.node("hub"), &acknowledged[0].1)
        .await
        .unwrap();
    cluster.node("edge-b").heal(cluster.node("hub"));
    cluster.node("edge-c").heal(cluster.node("hub"));

    // (a) and (c): every acknowledged edge write is at the hub, and the hub's writes are everywhere.
    let hub_names: BTreeSet<String> = {
        let all: BTreeSet<String> = acknowledged
            .iter()
            .flat_map(|(_, s)| s.iter().cloned())
            .collect();
        has_all(cluster.node("hub"), &all).await.unwrap();
        names(cluster.node("hub")).await.into_iter().collect()
    };
    for (edge, set) in &acknowledged {
        let at_hub = hub_names
            .iter()
            .filter(|n| n.starts_with(&format!("{edge}-")))
            .count();
        assert_eq!(at_hub, set.len(), "{edge}: no lost write");
    }
    for edge in EDGES {
        has_all(cluster.node(edge), &hub_day).await.unwrap();
        has_all(cluster.node(edge), &morning).await.unwrap();
    }

    // (b) no duplicate name across all nodes of this tenant: the union of every node's names holds
    // each name once per node, and the same name is the same document.
    for node in ["hub", "edge-a", "edge-b", "edge-c"] {
        let held = names(cluster.node(node)).await;
        let unique: BTreeSet<&String> = held.iter().collect();
        assert_eq!(
            held.len(),
            unique.len(),
            "{node}: no duplicate document name"
        );
    }

    // (d) the other tenant never appears here and the unenrolled edge holds nothing.
    let other = write(cluster.node("other-hub"), "other", 5).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    for node in ["hub", "edge-a", "edge-b", "edge-c", "stranger"] {
        let held: BTreeSet<String> = names(cluster.node(node)).await.into_iter().collect();
        assert!(
            other.iter().all(|n| !held.contains(n)),
            "{node}: no foreign tenant data"
        );
    }
    assert!(
        names(cluster.node("stranger")).await.is_empty(),
        "an unenrolled edge stays refused all day"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_day_three_hundred_documents_per_edge() {
    day(300).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "full profile: 5,000 documents per edge; run with L3_CHAOS=1 and --ignored"]
async fn full_day_five_thousand_documents_per_edge() {
    if std::env::var("L3_CHAOS").is_err() {
        println!("SKIPPED: set L3_CHAOS=1 for the full profile");
        return;
    }
    day(5000).await;
}
