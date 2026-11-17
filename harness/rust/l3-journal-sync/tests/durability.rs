//! The durability gate against real `spot serve` nodes, through their HTTP API (WP-RTB-01.T4).
//!
//! - **remote-sync happy path**: when a write's response returns, the journal is durable beyond
//!   the outbox head read before the request (the response waited for the chunk).
//! - **halt, with a store that cannot be written**: the first write is committed but unconfirmed
//!   (`503`, `journal.commit_outcome_unknown`), the next one is refused before it runs
//!   (`journal.halted`, `Retry-After`), reads still work, and writes resume when the store
//!   returns.
//! - **two sites**: one node's halted store never blocks the other.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use l3_journal_sync::{Client, Cluster, Node, NodeSpec, Role, wait_until};
use serde_json::json;
use spot_cli::services::durability::hosted as durability;
use spot_cli::services::journal::hosted as journal;
use spot_journal_store::STAGING_DIR;
use spot_postgres::{PgBackend, PgConfig};

async fn configure(node: &mut Node, target: &Path, on_unreachable: &str) {
    node.stop().await;
    // The key file the harness made for this node.
    let dir = node.root().join("keys");
    let path = node.root().join("common.toml");
    let mut text = l3_journal_sync::drop_secrets_table(&std::fs::read_to_string(&path).unwrap());
    text.push_str(&format!(
        "\n[services.secrets]\nkek_file = {:?}\n\
         [services.journal]\npolicy = \"remote-sync\"\non_unreachable = {on_unreachable:?}\n\
         batch_max_entries = 200\nship_interval_ms = 50\nseal_max_seconds = 1\n\
         [services.journal.target]\nkind = \"folder\"\npath = {:?}\n\
         [services.durability]\nwait_seconds = 2\nretry_after_seconds = 1\n",
        format!("{}/{{site}}.json", dir.display()),
        target.display().to_string(),
    ));
    std::fs::write(&path, text).unwrap();
    node.start().await;
}

fn target(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("l3_durability_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn head_of(node: &Node) -> u64 {
    let url = std::env::var("SPOT_TEST_PG_URL").expect("SPOT_TEST_PG_URL");
    let backend = PgBackend::connect_unchecked(
        PgConfig::from_url(&url)
            .unwrap()
            .with_dbname(node.db_name()),
    )
    .unwrap();
    let conn = backend.pool().get().await.unwrap();
    let row = conn
        .query_one(
            "SELECT coalesce((SELECT last_seq FROM spot_feed_state WHERE log = 'outbox'), 0)",
            &[],
        )
        .await
        .unwrap();
    u64::try_from(row.get::<_, i64>(0)).unwrap()
}

async fn create(client: &Client, owner: &str, code: &str) -> (u16, String) {
    let reply = client
        .create(
            "project",
            &json!({"code": code, "title": "durability", "status": "Active", "owner": owner,
                    "currency": "USD", "budget": "0.00"}),
        )
        .await
        .unwrap();
    (reply.status, reply.text())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_response_waits_for_the_journal_and_a_halted_site_does_not_block_another() {
    let mut cluster = Cluster::new();
    for name in ["dur-a", "dur-b"] {
        let spec = NodeSpec {
            name: name.to_owned(),
            role: Role::Standalone,
            extra_config_toml: String::new(),
        };
        if cluster.add_node(spec).await.is_none() {
            return;
        }
    }
    let (a_dir, b_dir) = (target("a"), target("b"));
    configure(cluster.node_mut("dur-a"), &a_dir, "halt").await;
    configure(cluster.node_mut("dur-b"), &b_dir, "degrade").await;
    let (a, b) = (cluster.node("dur-a"), cluster.node("dur-b"));
    let (a_client, b_client) = (a.client().unwrap(), b.client().unwrap());
    let (a_owner, b_owner) = (a.admin_id(), b.admin_id());
    // Happy path: after each response the journal is durable beyond the head read before it.
    for i in 0..5 {
        let before = head_of(a).await;
        let (status, text) = create(&a_client, &a_owner, &format!("D-{i}")).await;
        assert!((200..300).contains(&status), "HTTP {status}: {text}");
        let durable = journal(a.site()).unwrap().progress().durable().seq;
        assert!(
            durable > before,
            "durable {durable} must be beyond the head {before}"
        );
    }
    assert!(
        durability(a.site())
            .unwrap()
            .readiness_text()
            .starts_with("durability: remote-sync (halt)"),
        "{}",
        durability(a.site()).unwrap().readiness_text()
    );
    // A's store cannot be written (a process running as root writes anyway: nothing to prove).
    let staging = a_dir.join(STAGING_DIR);
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(staging.join("probe"), b"x").is_ok() {
            println!("SKIPPED: the process can write to a read-only folder (running as root)");
            let _ = std::fs::remove_file(staging.join("probe"));
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let (status, text) = create(&a_client, &a_owner, "D-unknown").await;
        assert_eq!(status, 503, "{text}");
        // The shipper's own sealing tick may have found the store down first: then the write is
        // refused up front as halted instead of ending with an unknown outcome.
        assert!(
            text.contains("journal.commit_outcome_unknown") || text.contains("journal.halted"),
            "{text}"
        );
        // The shipper now knows the store is down: the next write is refused before it runs.
        wait_until(Duration::from_secs(20), || async {
            journal(a.site()).is_some_and(|h| h.progress().now().degraded)
        })
        .await
        .unwrap();
        let (status, text) = create(&a_client, &a_owner, "D-refused").await;
        assert_eq!(status, 503, "{text}");
        assert!(text.contains("journal.halted"), "{text}");
        // B is untouched: its own store is fine.
        let (status, text) = create(&b_client, &b_owner, "B-1").await;
        assert!((200..300).contains(&status), "HTTP {status}: {text}");
        // Reads are never gated.
        assert_eq!(
            a_client
                .get("/api/v1/resource/project")
                .await
                .unwrap()
                .status,
            200
        );
        // The store returns: A resumes by itself.
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).unwrap();
        wait_until(Duration::from_secs(60), || async {
            let (status, _) = create(
                &a_client,
                &a_owner,
                &format!("D-back-{}", std::process::id()),
            )
            .await;
            (200..300).contains(&status)
        })
        .await
        .unwrap();
    }
}
