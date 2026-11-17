//! Personal-field protection and erasure against a real `spot serve` node (WP-RTB-01.T8).
//!
//! The sample `project.title` is declared personal with the project's `owner` as its data
//! subject. Projects are created over HTTP; the shipped journal holds no clear title; the
//! Administrator (listed in `erasers`) erases the owner subject, an unprivileged requester is
//! refused, and the wrapped-key table holds a tombstone for the subject (never a second key).
//!
//! What this file does not cover: the harness has no rebuild entry point yet (WP-RTB-01.T6), so
//! the placeholder-on-rebuild proof is the library test of `spot-journal-protect`.
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use l3_journal_sync::{Cluster, Node, NodeSpec, Role, wait_until};
use serde_json::json;
use spot_cli::services::journal::hosted as journal;
use spot_cli::services::journal_privacy::hosted as privacy;
use spot_postgres::{PgBackend, PgConfig};

const TITLE: &str = "Confidential grace hopper plan";

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
         [services.journal_privacy]\nerasers = [{:?}]\nforbid_erase_classes = []\n\
         [[services.journal_privacy.fields]]\nentity = \"project\"\npath = \"title\"\n\
         subject = \"owner\"\npurpose = \"planning\"\nretention_class = \"test_5y\"\n",
        format!("{}/{{site}}.json", dir.display()),
        target.display().to_string(),
        node.admin_id(),
    ));
    std::fs::write(&path, text).unwrap();
    node.start().await;
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn backend_of(node: &Node) -> PgBackend {
    let url = std::env::var("SPOT_TEST_PG_URL").expect("SPOT_TEST_PG_URL");
    PgBackend::connect_unchecked(
        PgConfig::from_url(&url)
            .unwrap()
            .with_dbname(node.db_name()),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_personal_field_is_sealed_in_the_journal_and_erasing_the_subject_destroys_its_key() {
    let mut cluster = Cluster::new();
    let spec = NodeSpec {
        name: "erase-a".to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    };
    if cluster.add_node(spec).await.is_none() {
        return;
    }
    let target = std::env::temp_dir().join(format!("l3_privacy_target_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    configure(cluster.node_mut("erase-a"), &target).await;
    let node = cluster.node("erase-a");
    let client = node.client().expect("running");
    let owner = node.admin_id();
    for i in 0..10 {
        let reply = client
            .create(
                "project",
                &json!({"code": format!("ER-{i}"), "title": TITLE, "status": "Active",
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
            p.durable.seq >= 10 && p.behind_entries == 0
        })
    })
    .await
    .unwrap();
    // Not even a plain-text title in the shipped journal (a weak check on compressed segments;
    // the library tests prove the lines the relay receives).
    for file in files_under(&target) {
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            !bytes.windows(TITLE.len()).any(|w| w == TITLE.as_bytes()),
            "a clear title in {}",
            file.display()
        );
    }
    let handle = privacy(node.site()).expect("the privacy module is hosted");
    let refused = handle
        .erase_subject(&owner, "somebody-else")
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "journal_privacy.permission_denied");
    let outcome = handle.erase_subject(&owner, &owner).await.unwrap();
    assert_eq!(outcome.erased_fields, 1);
    assert!(!outcome.is_partial());
    let conn = backend_of(node).pool().get().await.unwrap();
    let rows = conn
        .query("SELECT state FROM spot_journal_subject_key", &[])
        .await
        .unwrap();
    assert!(
        rows.iter().any(|r| r.get::<_, String>(0) == "destroyed"),
        "a tombstone, never a second key"
    );
}
