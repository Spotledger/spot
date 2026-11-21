//! Hostile nodes against the hosted hub (WP-RTB-02.T9): the product's security evidence for the
//! guarantees G1 to G4 of ADR-062 and the node model of ADR-064/065/067. For each hostile input:
//! the hub **refuses** it (a 4xx that is not "no such route"), the data is **unchanged** (the
//! document counts of both tenants before and after are equal) and **nothing else is
//! affected** (the other tenant's hub still answers).
//!
//! # What runs today
//!
//! The hosted sync listener (WP-RTB-02.T4 to T8) had not landed, so each test probes the hub and
//! prints `SKIPPED:` when sync is not served (it never replaces the check under test with a mock).
//! The inputs here need no node credentials and so run against the hub's own refusals: an
//! unenrolled certificate or token, a node presenting another tenant's site name, a malformed
//! frame, an oversize body, a replayed or forged enrolment code, a push naming another node's
//! partition. The cases that need an **enrolled** node's certificate (revoked or suspended
//! mid-connection, a non-owned record inside a valid bundle, a replayed valid batch, a hostile hub
//! against an edge's pinning) are `#[ignore]`d with the harness feature they wait for (an enrolment
//! helper and a TLS-capable proxy).
//!
//! The endpoint paths are those of the hosted hub (`/sync/v1/enrol`, `/sync/v1/push`,
//! `/sync/v1/pull`, `/sync/v1/ack`); if the hosted contract names them differently, change the
//! constants below.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_journal_sync::{Client, Cluster, Node, NodeSpec, Role};
use serde_json::{Value as Json, json};
use spot_postgres::{PgBackend, PgConfig};

const ENROL: &str = "/sync/v1/enrol";
const PUSH: &str = "/sync/v1/push";
const PULL: &str = "/sync/v1/pull";
const ACK: &str = "/sync/v1/ack";

fn spec(name: &str, role: Role) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role,
        extra_config_toml: String::new(),
    }
}

/// Two independent hubs (two tenants) and whether the first serves sync.
async fn hubs() -> Option<Cluster> {
    let mut cluster = Cluster::new();
    for name in ["hub", "other"] {
        if cluster.add_node(spec(name, Role::Hub)).await.is_none() {
            println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
            return None;
        }
    }
    let probe = anonymous(cluster.node("hub"));
    let served = l3_journal_sync::hub_is_wired(cluster.node("hub"))
        && matches!(
            probe.send("POST", ENROL, Some(&json!({}))).await,
            Ok(reply) if reply.status != 404 && reply.status != 501
        );
    if !served {
        println!("SKIPPED: the hub does not serve sync yet (WP-RTB-02.T4 to T8 hosting)");
        return None;
    }
    Some(cluster)
}

fn anonymous(node: &Node) -> Client {
    Client::new(node.client().unwrap().addr(), node.site())
}

/// How many `project` documents `node` holds: the data a hostile request must never change.
/// (A running host writes its own bookkeeping rows all the time, so whole-database fingerprints
/// are not stable while it serves; the documents are.)
async fn documents(node: &Node) -> i64 {
    let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
    let config = PgConfig::from_url(&url)
        .unwrap()
        .with_dbname(node.db_name());
    let backend = PgBackend::connect_unchecked(config).unwrap();
    let client = backend.pool().get().await.unwrap();
    client
        .query_one("SELECT count(*) FROM project", &[])
        .await
        .map(|row| row.get::<_, i64>(0))
        .unwrap_or(-1)
}

/// Sends one hostile request and asserts the hub refused it with a client error that is not "no
/// such route", that no document changed on either tenant, and that the other tenant still
/// answers.
async fn refused(
    cluster: &Cluster,
    client: &Client,
    method: &str,
    path: &str,
    body: Option<&Json>,
) {
    let before = (
        documents(cluster.node("hub")).await,
        documents(cluster.node("other")).await,
    );
    let reply = client.send(method, path, body).await.unwrap();
    assert!(
        (400..500).contains(&reply.status) && reply.status != 404,
        "{method} {path}: expected a refusal, got {} {}",
        reply.status,
        reply.text()
    );
    let after = (
        documents(cluster.node("hub")).await,
        documents(cluster.node("other")).await,
    );
    assert_eq!(
        before, after,
        "zero effect on data, on this tenant and on the other"
    );
    let other = cluster.node("other").client().unwrap();
    assert_eq!(
        other.health("ready").await.unwrap().status,
        200,
        "the other tenant still answers"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unenrolled_caller_is_refused_on_every_route() {
    let Some(cluster) = hubs().await else {
        return;
    };
    let client = anonymous(cluster.node("hub"));
    for (method, path) in [("POST", PUSH), ("POST", PULL), ("POST", ACK)] {
        refused(&cluster, &client, method, path, Some(&json!({}))).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_or_replayed_enrolment_code_is_refused() {
    let Some(cluster) = hubs().await else {
        return;
    };
    let client = anonymous(cluster.node("hub"));
    refused(
        &cluster,
        &client,
        "POST",
        ENROL,
        Some(&json!({"code": "NOT-A-REAL-CODE", "node": "edge-evil"})),
    )
    .await;
    // Twice: a replay of a refused code is refused again, never accepted on the second try.
    refused(
        &cluster,
        &client,
        "POST",
        ENROL,
        Some(&json!({"code": "NOT-A-REAL-CODE", "node": "edge-evil"})),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_presenting_another_tenants_site_name_is_refused() {
    let Some(cluster) = hubs().await else {
        return;
    };
    // The request reaches the hub of tenant `hub` but names tenant `other` as its site.
    let hub = cluster.node("hub");
    let client = Client::new(hub.client().unwrap().addr(), cluster.node("other").site());
    refused(
        &cluster,
        &client,
        "POST",
        PUSH,
        Some(&json!({"records": []})),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_framing_and_oversize_bodies_are_refused() {
    let Some(cluster) = hubs().await else {
        return;
    };
    let client = anonymous(cluster.node("hub"));
    // Not JSON where JSON is required (sent as a JSON string value of the wrong shape).
    refused(
        &cluster,
        &client,
        "POST",
        PUSH,
        Some(&json!("this is not a frame")),
    )
    .await;
    // A body far over any frame limit.
    let huge = json!({"records": vec!["x".repeat(1024); 4096]});
    refused(&cluster, &client, "POST", PUSH, Some(&huge)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_naming_another_nodes_partition_is_refused() {
    let Some(cluster) = hubs().await else {
        return;
    };
    let client = anonymous(cluster.node("hub"));
    refused(
        &cluster,
        &client,
        "POST",
        PUSH,
        Some(
            &json!({"partition": "branch-7", "records": [{"name": "project/evil", "version": 1}]}),
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_naming_another_nodes_destination_is_refused() {
    let Some(cluster) = hubs().await else {
        return;
    };
    let client = anonymous(cluster.node("hub"));
    refused(
        &cluster,
        &client,
        "POST",
        PULL,
        Some(&json!({"destination": "edge-other", "after": 0})),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs an enrolment helper in the harness (an enrolled node's certificate) to revoke or suspend mid-connection"]
async fn a_revoked_or_suspended_node_is_refused_mid_connection() {
    let Some(_cluster) = hubs().await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs an enrolled node and a bundle builder: a non-owned record inside a valid bundle applies nothing"]
async fn a_bundle_with_one_non_owned_record_applies_nothing() {
    let Some(_cluster) = hubs().await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs an enrolled node: a replay of an old valid batch is stale and never regresses a version"]
async fn a_replay_of_an_old_valid_batch_is_stale() {
    let Some(_cluster) = hubs().await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs a TLS-capable proxy and an edge with pinning: a CA-valid but unpinned hub certificate is refused by the edge"]
async fn an_unpinned_hub_certificate_is_refused_by_the_edge() {
    let Some(_cluster) = hubs().await else {
        return;
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gap: needs a hostile hub stub: a record outside the edge's catalogue is quarantined and a forged acknowledgement position is not believed"]
async fn a_hostile_hub_cannot_make_an_edge_apply_or_trust_a_position() {
    let Some(_cluster) = hubs().await else {
        return;
    };
}
