//! Journal snapshots against a real `spot serve` node, through its HTTP API (WP-RTB-01.T5).
//!
//! Projects are created over HTTP, the journal catches up, the snapshot service (configured to be
//! due at once) writes a snapshot, and the snapshot's position is at or before the shipped
//! watermark and its index is in the node's own journal prefix.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use l3_journal_sync::{Cluster, Node, NodeSpec, Role, wait_until};
use serde_json::json;
use spot_cli::services::journal::{FolderStore, JournalStore, hosted as journal};
use spot_cli::services::journal_snapshots::hosted as snapshots;

async fn configure(node: &mut Node, target: &Path) {
    node.stop().await;
    // The key file the harness made for this node.
    let dir = node.root().join("keys");
    let path = node.root().join("common.toml");
    let mut text = l3_journal_sync::drop_secrets_table(&std::fs::read_to_string(&path).unwrap());
    text.push_str(&format!(
        "\n[services.secrets]\nkek_file = {:?}\n\
         [services.journal]\nbatch_max_entries = 200\nship_interval_ms = 50\nseal_max_seconds = 1\n\
         [services.journal.target]\nkind = \"folder\"\npath = {:?}\n\
         [services.journal_snapshots]\nat = \"00:00\"\npoll_seconds = 1\nreach_timeout_seconds = 60\n",
        format!("{}/{{site}}.json", dir.display()),
        target.display().to_string(),
    ));
    std::fs::write(&path, text).unwrap();
    node.start().await;
}

/// `g1:s123` out of `snapshots: last g1:s123 0h ago (unverified), next 00:00`.
fn last_position(text: &str) -> Option<(u64, u64)> {
    let after = text.split("last g").nth(1)?;
    let (generation, rest) = after.split_once(":s")?;
    let seq: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((generation.parse().ok()?, seq.parse().ok()?))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snapshot_is_written_at_or_before_the_shipped_watermark() {
    let mut cluster = Cluster::new();
    let spec = NodeSpec {
        name: "snap-a".to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    };
    if cluster.add_node(spec).await.is_none() {
        return;
    }
    let target = std::env::temp_dir().join(format!("l3_snapshot_target_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    configure(cluster.node_mut("snap-a"), &target).await;
    let node = cluster.node("snap-a");
    let client = node.client().expect("running");
    let owner = node.admin_id();
    for i in 0..50 {
        let reply = client
            .create(
                "project",
                &json!({"code": format!("SN-{i}"), "title": "snapshot", "status": "Active",
                        "owner": owner, "currency": "USD", "budget": "0.00"}),
            )
            .await
            .unwrap();
        assert!(
            (200..300).contains(&reply.status),
            "HTTP {}: {}",
            reply.status,
            reply.text()
        );
    }
    wait_until(Duration::from_secs(120), || async {
        snapshots(node.site()).is_some_and(|h| last_position(&h.readiness_text()).is_some())
    })
    .await
    .unwrap_or_else(|e| {
        let text = snapshots(node.site()).map(|h| h.readiness_text());
        panic!("{e}: {text:?}")
    });
    let (generation, seq) =
        last_position(&snapshots(node.site()).unwrap().readiness_text()).unwrap();
    let durable = journal(node.site()).unwrap().progress().durable();
    assert!(
        (generation, seq) <= (durable.generation, durable.seq),
        "the snapshot (g{generation}:s{seq}) is at or before the shipped watermark (g{}:s{})",
        durable.generation,
        durable.seq
    );
    // The index is in this node's own journal prefix.
    let store = FolderStore::new(&target).unwrap();
    let listed = store
        .list(&format!("{}/snapshots/", node.site()), None, 1_000)
        .await
        .unwrap();
    assert!(
        !listed.is_empty(),
        "snapshot objects under the node's own prefix"
    );
    assert!(
        listed
            .iter()
            .all(|o| o.key.starts_with(&format!("{}/", node.site())))
    );
}
