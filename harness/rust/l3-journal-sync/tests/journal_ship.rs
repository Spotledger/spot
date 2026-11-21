//! The journal shipper against real `spot serve` nodes, through their HTTP API (WP-RTB-01.T3).
//!
//! - **1,000 documents** created over HTTP are in the journal: the shipper catches up with the
//!   outbox, every sealed position is contiguous, and the chain verifies against the signing key
//!   derived from the site's key file.
//! - **A tampered segment byte** is detected by verification, `JOURNAL_GAP` is raised through
//!   the conditions service and the shipper stops writing.
//! - **Two sites on one folder root**: neither can read, list or overwrite the other's prefix.
//!
//! A node is started the way an operator does: a key file for `[services.secrets]` and a
//! `[services.journal]` table appended to `common.toml`, then a restart (the harness's
//! `Node::stop` / `Node::start`).
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use l3_journal_sync::tamper::flip_byte;
use l3_journal_sync::{Client, Cluster, Node, NodeSpec, Role, wait_until};
use serde_json::json;
use spot_cli::services::journal::{
    ChainChecker, ChainState, FolderStore, JournalKeys, JournalStore, LocalKeyring, StoreChecker,
    TenantScoped, hosted,
};
use spot_journal_store::StoreError;
use spot_postgres::{PgBackend, PgConfig};

fn spec(name: &str) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    }
}

/// Where a node's key file lives: outside the site folders, one file per site.
fn key_dir(node: &Node) -> PathBuf {
    // The key file the harness made for this node.
    node.root().join("keys")
}

/// Restarts `node` with a key file and the journal pointed at `target`.
async fn configure(node: &mut Node, target: &Path) {
    node.stop().await;
    let dir = key_dir(node);
    let path = node.root().join("common.toml");
    let mut text = l3_journal_sync::drop_secrets_table(&std::fs::read_to_string(&path).unwrap());
    text.push_str(&format!(
        "\n[services.secrets]\nkek_file = {:?}\n\
         [services.journal]\npolicy = \"remote-sync\"\non_unreachable = \"degrade\"\n\
         batch_max_entries = 500\nship_interval_ms = 50\nverify_every_seconds = 1\n\
         seal_max_seconds = 1\nrpo_grace_seconds = 60\n\
         [services.journal.target]\nkind = \"folder\"\npath = {:?}\n",
        format!("{}/{{site}}.json", dir.display()),
        target.display().to_string(),
    ));
    std::fs::write(&path, text).unwrap();
    node.start().await;
}

fn backend_of(node: &Node) -> PgBackend {
    let url = std::env::var("SPOT_TEST_PG_URL").expect("SPOT_TEST_PG_URL");
    let config = PgConfig::from_url(&url)
        .unwrap()
        .with_dbname(node.db_name());
    PgBackend::connect_unchecked(config).unwrap()
}

/// The signing material of `node`'s journal, derived from its key file exactly as the host does.
async fn keys_of(node: &Node) -> JournalKeys {
    let kek = key_dir(node).join(format!("{}.json", node.site()));
    let ring = LocalKeyring::open(&kek).await.unwrap();
    JournalKeys::derive(Arc::new(ring)).await.unwrap()
}

async fn caught_up(node: &Node, at_least: u64) -> bool {
    let Some(handle) = hosted(node.site()) else {
        return false;
    };
    let progress = handle.progress().now();
    progress.durable.seq >= at_least && progress.behind_entries == 0 && progress.writer
}

fn checker(
    node: &Node,
    folder: &Path,
    keys: &JournalKeys,
) -> StoreChecker<TenantScoped<FolderStore>> {
    StoreChecker::new(
        TenantScoped::new(FolderStore::new(folder).unwrap(), node.site()).unwrap(),
        keys.verifying(),
        node.site(),
        node.site(),
    )
    .unwrap()
}

async fn segment_files(folder: &Path, site: &str) -> Vec<PathBuf> {
    let store = FolderStore::new(folder).unwrap();
    let mut out: Vec<PathBuf> = store
        .list(&format!("{site}/segments/"), None, 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| folder.join(entry.key))
        .collect();
    out.sort();
    out
}

/// The next project code number: codes are unique, and a test creates projects in several calls.
static NEXT_CODE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

async fn create_projects(client: &Client, owner: &str, count: u32) {
    for _ in 0..count {
        let i = NEXT_CODE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reply = client
            .create(
                "project",
                &json!({"code": format!("JS-{i}"), "title": "journal", "status": "Active",
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thousand_documents_reach_the_journal_and_a_tampered_segment_raises_journal_gap() {
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("ship-a")).await.is_none() {
        return;
    }
    let target = tempfile_dir("l3_journal_target_a");
    {
        let node = cluster.node_mut("ship-a");
        let folder = target.clone();
        configure(node, &folder).await;
    }
    let node = cluster.node("ship-a");
    let client = node.client().expect("running");
    create_projects(&client, &node.admin_id(), 1_000).await;
    wait_until(Duration::from_secs(120), || async {
        caught_up(node, 1_000).await
    })
    .await
    .unwrap_or_else(|e| {
        let text = hosted(node.site()).map(|h| h.readiness_text());
        panic!("{e}: {text:?}")
    });
    let keys = keys_of(node).await;
    let clean = checker(node, &target, &keys);
    assert_eq!(clean.check(1).await.unwrap(), ChainState::Clean);
    let segments = segment_files(&target, node.site()).await;
    assert!(!segments.is_empty(), "sealed segments are in the folder");

    // One flipped byte in a sealed segment: verification calls the chain damaged.
    flip_byte(&segments[0], 40).unwrap();
    assert!(matches!(
        clean.check(1).await.unwrap(),
        ChainState::Damaged { .. }
    ));
    // The next document makes the shipper verify before it writes: JOURNAL_GAP is raised and the
    // shipper stops.
    create_projects(&client, &node.admin_id(), 1).await;
    let backend = backend_of(node);
    wait_until(Duration::from_secs(90), || {
        let backend = backend.clone();
        async move {
            let conn = backend.pool().get().await.unwrap();
            conn.query(
                "SELECT 1 FROM spot_condition_open WHERE id = 'JOURNAL_GAP'",
                &[],
            )
            .await
            .map(|rows| !rows.is_empty())
            .unwrap_or(false)
        }
    })
    .await
    .unwrap();
    let progress = hosted(node.site()).unwrap().progress().now();
    assert!(
        progress.stopped.is_some(),
        "the shipper stopped on the damage"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sites_on_one_folder_root_cannot_reach_each_others_prefix() {
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("ship-b")).await.is_none() {
        return;
    }
    if cluster.add_node(spec("ship-c")).await.is_none() {
        return;
    }
    let shared = tempfile_dir("l3_journal_target_shared");
    for name in ["ship-b", "ship-c"] {
        configure(cluster.node_mut(name), &shared).await;
    }
    for name in ["ship-b", "ship-c"] {
        let node = cluster.node(name);
        create_projects(&node.client().expect("running"), &node.admin_id(), 20).await;
    }
    for name in ["ship-b", "ship-c"] {
        let node = cluster.node(name);
        wait_until(Duration::from_secs(60), || async {
            caught_up(node, 20).await
        })
        .await
        .unwrap();
    }
    let (b, c) = (cluster.node("ship-b"), cluster.node("ship-c"));
    let store = FolderStore::new(&shared).unwrap();
    let b_objects = store
        .list(&format!("{}/", b.site()), None, 10_000)
        .await
        .unwrap();
    let c_objects = store
        .list(&format!("{}/", c.site()), None, 10_000)
        .await
        .unwrap();
    assert!(!b_objects.is_empty() && !c_objects.is_empty());
    assert!(
        b_objects
            .iter()
            .all(|o| o.key.starts_with(&format!("{}/", b.site())))
    );
    assert!(
        c_objects
            .iter()
            .all(|o| o.key.starts_with(&format!("{}/", c.site())))
    );
    // A scoped view of one site refuses the other's keys, listings and writes.
    let scoped = TenantScoped::new(FolderStore::new(&shared).unwrap(), b.site()).unwrap();
    let foreign = c_objects[0].key.clone();
    assert!(matches!(
        scoped.get(&foreign).await,
        Err(StoreError::KeyInvalid(_))
    ));
    assert!(matches!(
        scoped.put_new(&foreign, vec![0]).await,
        Err(StoreError::KeyInvalid(_))
    ));
    assert!(matches!(
        scoped.list(&format!("{}/", c.site()), None, 10).await,
        Err(StoreError::KeyInvalid(_))
    ));
    // Each chain verifies under its own key.
    for node in [b, c] {
        let keys = keys_of(node).await;
        assert_eq!(
            checker(node, &shared, &keys).check(1).await.unwrap(),
            ChainState::Clean
        );
    }
}

fn tempfile_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
