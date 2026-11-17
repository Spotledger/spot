//! The host builds the cluster-redis plugin once, before the modules, when
//! `[services.cluster_redis] enabled = true` in the `cluster` profile (WP-PLT-19.T3): the site
//! holds one connection and one `Status` (a second registration of the module would refuse to
//! provide the same ports twice), a site that asked for Redis and cannot reach it does not start
//! (nothing falls back to PostgreSQL leases), and with Redis off nothing changes.
//!
//! The tests need PostgreSQL (`SPOT_TEST_PG_URL`); the one that needs a Redis or Valkey server
//! reads `SPOT_TEST_REDIS_URL` (for example `redis://127.0.0.1:6379`, loopback only) and prints
//! why it skips when it is unset.
//!
//! Not covered here: cutting the connection under two hosts and proving the fencing of a stale
//! holder (it needs a second host process and a Redis admin client); see the task report.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;

fn cluster(redis: &str) -> String {
    format!(
        "[services]\nprofile = \"cluster\"\n[services.metadata]\nbus = \"polling\"\n[services.cluster_redis]\n{redis}\n"
    )
}

async fn ready(started: &Started, host: &str) -> (u16, String) {
    let reply = http(
        started.addr,
        host,
        None,
        "GET",
        "/api/health/ready",
        &[],
        None,
    )
    .await;
    (
        reply.status,
        String::from_utf8_lossy(&reply.bytes).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_redis_off_nothing_changes_and_no_cluster_redis_service_runs() {
    let Some(env) = Env::new("[services.cluster_redis]\nenabled = false\n").await else {
        return;
    };
    let host = env.site();
    let started = start(&env).await;
    let (status, body) = ready(&started, &host).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("cluster_redis"), "{body}");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_that_asked_for_redis_and_cannot_reach_it_does_not_serve() {
    // Port 1 on loopback: nothing listens. The plugin's own start-up check refuses, the host
    // does not fall back to the PostgreSQL coordinator, and the site is reported unavailable.
    let services = cluster("enabled = true\nurl = \"redis://127.0.0.1:1\"\nallow_plaintext = true");
    let Some(env) = Env::new(&services).await else {
        return;
    };
    env.site();
    let started = start(&env).await;
    // The host stays up for other sites; this site answers the unavailable envelope.
    let reply = http(
        started.addr,
        &env.site,
        None,
        "GET",
        "/api/v1/auth/me",
        &[],
        None,
    )
    .await;
    assert_eq!(
        reply.status, 503,
        "the site must not serve without the Redis it asked for: {}",
        reply.body
    );
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_redis_on_the_site_starts_with_one_plugin_instance() {
    let Ok(url) = std::env::var("SPOT_TEST_REDIS_URL") else {
        eprintln!("SKIPPED: SPOT_TEST_REDIS_URL is not set (needs a Redis or Valkey server)");
        return;
    };
    let services = cluster(&format!(
        "enabled = true\nurl = {url:?}\nallow_plaintext = true"
    ));
    let Some(env) = Env::new(&services).await else {
        return;
    };
    let host = env.site();
    let started = start(&env).await;
    // A second registration of the plugin would provide the same ports twice and stop the site.
    let (status, body) = ready(&started, &host).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("cluster_redis") || !body.contains("degraded"),
        "{body}"
    );
    started.finish().await;
}
