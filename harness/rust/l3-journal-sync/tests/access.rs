//! The journal access surface over HTTP against a real `spot serve` node (WP-JRN-09.T4).
//!
//! What is proved today: a node whose journal is not hosted (the journal service, WP-RTB-01.T3,
//! provides the store and the manifest keys the access module needs) answers all three
//! operations `501 service.not_configured` with `Cache-Control: private, no-store`, and never
//! `200` with an empty history, to an administrator and to a stranger alike; an unauthenticated
//! request is `401`. The full flow (a granted auditor reads the manifest and every listed
//! segment, a Range request resumed after a cut, two sites on one store) is proved over the real
//! database in `crates/framework/spot-cli/tests/journal_access_host.rs`; once the journal
//! service provides its ports this file gains the same flow over HTTP (see the task report).
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_journal_sync::{Cluster, NodeSpec, Role};

fn spec(name: &str) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_without_a_hosted_journal_answers_501_not_configured() {
    let mut cluster = Cluster::new();
    let Some(node) = cluster.add_node(spec("solo")).await else {
        println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
        return;
    };
    let client = node.client().expect("the node signed its administrator in");
    for path in [
        "/api/v1/journal",
        "/api/v1/journal/manifest/1",
        "/api/v1/journal/segments/1/1-100",
    ] {
        let reply = client.get(path).await.unwrap();
        assert_eq!(reply.status, 501, "{path}: {}", reply.text());
        assert_eq!(reply.json()["error"]["code"], "service.not_configured");
        assert!(
            reply
                .headers
                .iter()
                .any(|(k, v)| k == "cache-control" && v == "private, no-store"),
            "{path}: private, no-store"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unauthenticated_request_is_401_and_a_hostile_path_is_never_a_200() {
    let mut cluster = Cluster::new();
    let Some(node) = cluster.add_node(spec("solo")).await else {
        println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
        return;
    };
    let admin = node.client().expect("the node signed its administrator in");
    let anonymous = l3_journal_sync::Client::new(admin.addr(), node.site());
    let reply = anonymous.get("/api/v1/journal").await.unwrap();
    assert_eq!(reply.status, 401, "{}", reply.text());
    for path in [
        "/api/v1/journal/segments/%2e%2e/1-100",
        "/api/v1/journal/segments/1/00000000000000000001-00000000000000000100",
        "/api/v1/journal/manifest/..",
    ] {
        let reply = admin.get(path).await.unwrap();
        assert_ne!(reply.status, 200, "{path}");
    }
}
