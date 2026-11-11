//! The capture invariant against real `spot serve` nodes, through their HTTP API (WP-JRN-02.T3).
//!
//! - **200 mixed operations** (create, patch, delete, post, cancel) through the API of one node,
//!   then the whole change feed compared with the whole journal ([`CaptureGate::run_once`]): zero
//!   findings for what the API wrote.
//! - **Two sites**: a bypass write in site A raises `CAPTURE_MISMATCH` on A only, and site B's
//!   own pass raises nothing and holds no such condition.
//!
//! # What this file works around
//!
//! 1. `Client::send` carries no header argument and a PATCH, DELETE, post or cancel needs
//!    `If-Match`. `src/` is not an owned path of this task, so [`with_if_match`] adds the header
//!    through the request line the client formats (the node's own parser reads it as a header).
//! 2. A freshly provisioned site's seed roles, role assignments and administrator are inserted
//!    by the installer directly, not through the document engine, so the invariant reports them
//!    (that is a finding about provisioning, not about this task: see the PR's open questions).
//!    This file excludes exactly those three entities, with an owner and a reason, so the
//!    assertions are about what the API wrote.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention).
//! SQL is written in two pieces (G5).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use l3_journal_sync::{Client, Cluster, Node, NodeSpec, Reply, Role};
use serde_json::json;
use spot_cli::services::capture_invariant::{CaptureGate, Exclusions, FindingKind, hosted};
use spot_cli::services::conditions::{ChangeFeed, FeedPosition};
use spot_postgres::{PgBackend, PgConfig};

fn spec(name: &str) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    }
}

/// The settings every node runs the service with: a long interval (the test drives each pass) and
/// a one second lag.
const SERVICE_TOML: &str =
    "\n[services.capture_invariant]\nenabled = true\ninterval_seconds = 3600\nlag_seconds = 1\n";

/// Restarts `node` with the capture invariant configured for the test.
async fn configure(node: &mut Node) {
    node.stop().await;
    let path = node.root().join("common.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(SERVICE_TOML);
    std::fs::write(&path, text).unwrap();
    node.start().await;
}

/// A pool on `node`'s own database.
fn backend_of(node: &Node) -> PgBackend {
    let url = std::env::var("SPOT_TEST_PG_URL").expect("SPOT_TEST_PG_URL");
    let config = PgConfig::from_url(&url)
        .unwrap()
        .with_dbname(node.db_name());
    PgBackend::connect_unchecked(config).unwrap()
}

/// `path` with an `If-Match` header for `etag` riding on the request line.
fn with_if_match(path: &str, etag: &str) -> String {
    format!("{path} HTTP/1.1\r\nIf-Match: {etag}\r\nX-L3-Pad: 1")
}

fn etag(reply: &Reply) -> String {
    reply
        .headers
        .iter()
        .find(|(name, _)| name == "etag")
        .map(|(_, value)| value.clone())
        .expect("a document response carries its ETag")
}

fn ok(reply: &Reply) {
    assert!(
        (200..300).contains(&reply.status),
        "HTTP {}: {}",
        reply.status,
        reply.text()
    );
}

/// What site provisioning writes outside the engine. See the module documentation.
fn provisioning() -> Exclusions {
    let mut out = Exclusions::new();
    for entity in ["role", "role_assignment", "user"] {
        out = out
            .local(
                entity,
                "l3-journal-sync",
                "site_provisioning_writes_directly",
            )
            .unwrap();
    }
    out
}

/// Publishes both logs, then waits out the lag so the next pass can judge.
async fn publish(backend: &PgBackend) {
    let _ = backend.read_changes(FeedPosition::START, 1).await;
    let _ = backend.read_outbox(FeedPosition::START, 1).await;
}

async fn count(backend: &PgBackend, head: &str, tail: &str) -> i64 {
    let client = backend.pool().get().await.unwrap();
    let sql = format!("{head}{tail}");
    client.query_one(&sql, &[]).await.unwrap().get(0)
}

#[tokio::test]
async fn two_hundred_mixed_operations_leave_no_finding() {
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("cap-a")).await.is_none() {
        return;
    }
    let node = cluster.node("cap-a");
    let client: Client = node.client().expect("running");
    let owner = node.admin_id();

    // A project, a task under it, and the plan of operations.
    let project = client
        .create(
            "project",
            &json!({"code": "CAP-base", "title": "capture", "status": "Active", "owner": owner,
                    "currency": "USD", "budget": "0.00"}),
        )
        .await
        .unwrap();
    ok(&project);
    let project_id = project.id().unwrap();
    let task = client
        .create(
            "task",
            &json!({"subject": "capture task", "project": project_id}),
        )
        .await
        .unwrap();
    ok(&task);
    let task_id = task.id().unwrap();

    let mut projects: Vec<(String, String)> = Vec::new(); // (id, etag)
    let mut drafts: Vec<(String, String)> = Vec::new();
    let mut posted: Vec<(String, String)> = Vec::new();
    let mut done = [0_u32; 5];
    for i in 0..200_u32 {
        match i % 10 {
            0 | 5 => {
                let reply = client
                    .create(
                        "project",
                        &json!({"code": format!("CAP-{i}"), "title": "p", "status": "Active",
                                "owner": owner, "currency": "USD", "budget": "0.00"}),
                    )
                    .await
                    .unwrap();
                ok(&reply);
                projects.push((reply.id().unwrap(), etag(&reply)));
                done[0] += 1;
            }
            1 | 6 => {
                if let Some((id, tag)) = projects.last_mut() {
                    let reply = client
                        .send(
                            "PATCH",
                            &with_if_match(&format!("/api/v1/resource/project/{id}"), tag),
                            Some(&json!({"title": format!("patched {i}")})),
                        )
                        .await
                        .unwrap();
                    ok(&reply);
                    *tag = etag(&reply);
                    done[1] += 1;
                }
            }
            2 | 7 => {
                if let Some((id, tag)) = projects.pop() {
                    let reply = client
                        .send(
                            "DELETE",
                            &with_if_match(&format!("/api/v1/resource/project/{id}"), &tag),
                            None,
                        )
                        .await
                        .unwrap();
                    ok(&reply);
                    done[2] += 1;
                }
            }
            3 | 8 => {
                let reply = client
                    .create(
                        "time_entry",
                        &json!({"task": task_id, "person": owner, "work_date": "2026-10-20",
                                "hours": "1.00"}),
                    )
                    .await
                    .unwrap();
                ok(&reply);
                drafts.push((reply.id().unwrap(), etag(&reply)));
            }
            4 => {
                if let Some((id, tag)) = drafts.pop() {
                    let reply = client
                        .send(
                            "POST",
                            &with_if_match(&format!("/api/v1/resource/time_entry/{id}/post"), &tag),
                            None,
                        )
                        .await
                        .unwrap();
                    ok(&reply);
                    posted.push((id, etag(&reply)));
                    done[3] += 1;
                }
            }
            _ => {
                if let Some((id, tag)) = posted.pop() {
                    let reply = client
                        .send(
                            "POST",
                            &with_if_match(
                                &format!("/api/v1/resource/time_entry/{id}/cancel"),
                                &tag,
                            ),
                            None,
                        )
                        .await
                        .unwrap();
                    ok(&reply);
                    done[4] += 1;
                }
            }
        }
    }
    assert!(
        done.iter().all(|n| *n > 0),
        "every kind of operation ran: {done:?}"
    );

    let backend = backend_of(node);
    publish(&backend).await;
    let report = CaptureGate::new()
        .with_exclusions(provisioning())
        .run_once(&backend.pool())
        .await
        .unwrap();
    assert!(
        report.feed_entries > 100,
        "{} feed entries",
        report.feed_entries
    );
    assert!(
        report.is_clean(),
        "findings: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.entity.clone(), f.version))
            .collect::<Vec<_>>()
    );
    assert_eq!(report.count(FindingKind::MissingStateEvent), 0);
}

#[tokio::test]
async fn a_bypass_in_one_site_raises_only_that_sites_condition() {
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("cap-x")).await.is_none() {
        return;
    }
    cluster
        .add_node(spec("cap-y"))
        .await
        .expect("a second site");
    for name in ["cap-x", "cap-y"] {
        configure(cluster.node_mut(name)).await;
    }

    // The same traffic in both sites.
    for name in ["cap-x", "cap-y"] {
        let node = cluster.node(name);
        let client = node.client().unwrap();
        let owner = node.admin_id();
        for i in 0..5 {
            ok(&client
                .create(
                    "project",
                    &json!({"code": format!("{name}-{i}"), "title": "t", "status": "Active",
                            "owner": owner, "currency": "USD", "budget": "0.00"}),
                )
                .await
                .unwrap());
        }
    }

    // A bypass write in site X only: a stored version bump with SQL.
    let x = cluster.node("cap-x");
    let x_backend = backend_of(x);
    {
        let client = x_backend.pool().get().await.unwrap();
        let sql = format!(
            "{}{}",
            "UPDATE project ", "SET version = version + 1 WHERE code = 'cap-x-0'"
        );
        client.execute(&sql, &[]).await.unwrap();
    }

    let service_x = hosted(x.site()).expect("site x hosts the invariant");
    let service_y = hosted(cluster.node("cap-y").site()).expect("site y hosts the invariant");
    // The first pass publishes, the second (after the lag) judges.
    let _ = service_x.run_pass().await;
    let _ = service_y.run_pass().await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let _ = service_x.run_pass().await.unwrap();
    let _ = service_y.run_pass().await.unwrap();
    // Let both condition engines store what they hold.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let about_projects = (
        "SELECT count(*)::bigint ",
        "FROM spot_condition_open WHERE id = 'CAPTURE_MISMATCH' AND params->>'record' LIKE 'project/%'",
    );
    assert_eq!(
        count(&x_backend, about_projects.0, about_projects.1).await,
        1,
        "site X holds the condition of its own bypassed record"
    );
    let y_backend = backend_of(cluster.node("cap-y"));
    assert_eq!(
        count(&y_backend, about_projects.0, about_projects.1).await,
        0,
        "site Y holds none"
    );
}
