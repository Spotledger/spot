//! Event history from the journal against a real `spot serve` node (WP-JRN-09.T3).
//!
//! Projects are created over HTTP, the journal catches up, and the node's hosted history reader
//! (the one the stream engine's adapter uses) returns every `doc_state` event of those projects
//! in order from position 0, with no gap and no duplicate position.
//!
//! **Not covered here, and why.** The brief's HTTP cases (a consumer registered with `classes` and
//! `entities`, `served_from` in pull and SSE, a denied principal, a principal without a field, the
//! abandoned generation answering `410`) need `routes/events.rs` and `services/events.rs` to call
//! the helpers of this task, and those files (WP-INT-02.T4) are not in its paths: see the task
//! report. The selector and `served_from` rules are proved in `spot-http`'s `event_history` test.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use l3_journal_sync::{Cluster, Node, NodeSpec, Role, wait_until};
use serde_json::json;
use spot_cli::services::event_history::{Position, Selector, hosted as history};
use spot_cli::services::journal::hosted as journal;

async fn configure(node: &mut Node, target: &Path) {
    node.stop().await;
    // The key file the harness made for this node.
    let dir = node.root().join("keys");
    let path = node.root().join("common.toml");
    let mut text = l3_journal_sync::drop_secrets_table(&std::fs::read_to_string(&path).unwrap());
    text.push_str(&format!(
        "\n[services.secrets]\nkek_file = {:?}\n\
         [services.journal]\nbatch_max_entries = 200\nship_interval_ms = 50\nseal_max_seconds = 1\n\
         [services.journal.target]\nkind = \"folder\"\npath = {:?}\n",
        format!("{}/{{site}}.json", dir.display()),
        target.display().to_string(),
    ));
    std::fs::write(&path, text).unwrap();
    node.start().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hosted_history_reader_returns_every_state_event_in_order() {
    let mut cluster = Cluster::new();
    let spec = NodeSpec {
        name: "hist-a".to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    };
    if cluster.add_node(spec).await.is_none() {
        return;
    }
    let target = std::env::temp_dir().join(format!("l3_history_target_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    configure(cluster.node_mut("hist-a"), &target).await;
    let node = cluster.node("hist-a");
    let client = node.client().expect("running");
    let owner = node.admin_id();
    for i in 0..60 {
        let reply = client
            .create(
                "project",
                &json!({"code": format!("H-{i}"), "title": "history", "status": "Active",
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
    wait_until(Duration::from_secs(90), || async {
        journal(node.site()).is_some_and(|h| {
            let p = h.progress().now();
            p.durable.seq >= 60 && p.behind_entries == 0
        })
    })
    .await
    .unwrap();
    let reader = history(node.site()).expect("the history reader is hosted");
    let all: &Selector = &|_| true;
    let mut after = Position {
        generation: 0,
        seq: 0,
    };
    let mut positions = Vec::new();
    let mut states = 0;
    for _ in 0..100 {
        let page = reader.read_after(after, 200, None, all).await.unwrap();
        for envelope in &page.envelopes {
            positions.push(envelope.position.seq);
            if envelope.topic.as_str() == "doc_state" {
                states += 1;
            }
        }
        after = page.next_after;
        if page.reached_outbox {
            break;
        }
    }
    assert!(states >= 60, "a doc_state event per project: {states}");
    let expected: Vec<u64> = (1..=positions.len() as u64).collect();
    assert_eq!(
        positions, expected,
        "in order, no gap, no duplicate position"
    );
}
