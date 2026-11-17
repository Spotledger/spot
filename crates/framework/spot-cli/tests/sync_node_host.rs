//! The edge sync service as the host sees it (WP-RTB-02.T5): the plan, the role row, and the
//! offline-is-normal readiness. The hosted checks over a real `spot serve` with PostgreSQL on both
//! sides are owed to the bundle run (they need `SPOT_TEST_PG_URL`); everything here needs no database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use spot_cli::services::node_role::{self, Role};
use spot_cli::services::sync_node::{self, Settings};
use spot_cli::services::{Level, Service};
use spot_sync_node_host::{LinkState, StatusHandle};

fn role(url: &str) -> node_role::Settings {
    node_role::Settings {
        role: Some("edge".into()),
        hub_url: Some(url.into()),
        ..node_role::Settings::default()
    }
}

#[test]
fn the_module_is_edge_only_and_matches_the_role_table() {
    assert_eq!(sync_node::ROLES, ["edge"]);
    assert!(node_role::service_set(Role::Edge).contains(&"sync_node"));
    assert!(!node_role::service_set(Role::Hub).contains(&"sync_node"));
    const { assert!(sync_node::ORDER > node_role::ORDER) };
}

#[test]
fn the_plan_resolves_the_hub_and_refuses_what_cannot_be_pinned() {
    let plan = sync_node::plan(
        &Settings::default(),
        &role("https://hub.example:7443/x"),
        "khi",
        Some("edge-khi#branch_01"),
        Path::new("/etc/spot"),
    )
    .unwrap();
    assert_eq!(plan.address, "hub.example:7443");
    assert_eq!(plan.server_name, "hub.example");
    assert_eq!(plan.hub_source.as_str(), "khi#hub");
    assert!(plan.identity_dir.ends_with("sync-node/khi"));
    let default_port = sync_node::plan(
        &Settings::default(),
        &role("https://hub.example"),
        "khi",
        Some("edge-khi#branch_01"),
        Path::new("/etc/spot"),
    )
    .unwrap();
    assert_eq!(default_port.address, "hub.example:443");
    for bad in ["http://hub.example", "https://", "hub.example"] {
        assert!(
            sync_node::plan(
                &Settings::default(),
                &role(bad),
                "khi",
                Some("edge-khi#branch_01"),
                Path::new("/")
            )
            .is_err(),
            "{bad}"
        );
    }
    assert!(
        sync_node::plan(
            &Settings::default(),
            &role("https://h"),
            "khi",
            None,
            Path::new("/")
        )
        .is_err()
    );
    let inverted = Settings {
        backoff_min_secs: Some(10),
        backoff_max_secs: Some(1),
        ..Settings::default()
    };
    assert!(
        sync_node::plan(
            &inverted,
            &role("https://h"),
            "khi",
            Some("a#b"),
            Path::new("/")
        )
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unwired_service_says_why_and_an_offline_one_is_never_not_ready() {
    let status = StatusHandle::new("hub-1", LinkState::Connecting);
    let unwired = sync_node::SyncNodeService::new(status.clone(), Some("not wired".into()), None);
    assert_eq!(unwired.level(), Level::Optional);
    assert_eq!(unwired.check().await, Err("not wired".to_owned()));

    let service = sync_node::SyncNodeService::new(status.clone(), None, None);
    status.offline(100, "sync_http.unavailable");
    assert!(
        service.check().await.is_ok(),
        "offline must not fail the check"
    );
    let line = service
        .status()
        .snapshot()
        .readiness(100 + 3 * 3600 + 12 * 60);
    assert_eq!(line, "sync_node: offline 3h12m, 0 changes queued");

    // A stop ends the task cleanly.
    let (tx, rx) = tokio::sync::watch::channel(false);
    let running = tokio::spawn(async move { service.run(rx).await });
    tx.send(true).unwrap();
    // `service` moved into the task; its result is Ok.
    assert!(running.await.unwrap().is_ok());
}
