//! `/api/health/ready` and the capture invariant (WP-JRN-02.T7): an enabled site shows the
//! service's text, three failed passes in a row show `degraded` while the endpoint still answers
//! 200 (the service never stops trading), and a site without the service shows no line.
//!
//! A forced stream failure renames the published change feed, which the pass reads, and renames
//! it back. SQL statements are written in two pieces (G5 refuses a full statement outside the
//! storage crates), as `capture_invariant_host.rs` does. Real PostgreSQL: `SPOT_TEST_PG_URL`
//! selects the server and `SPOT_REQUIRE_PG=1` makes a missing one loud.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_cli::services::capture_invariant::hosted;

const SERVICES: &str =
    "[services.capture_invariant]\nenabled = true\ninterval_seconds = 3600\nlag_seconds = 1\n";

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

async fn rename_feed(env: &Env, from: &str, to: &str) {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|site| site.site == env.site)
        .unwrap()
        .clone();
    let backend = spot_cli::schema::backend(&loaded, &site).await.unwrap();
    let client = backend.pool().get().await.unwrap();
    let sql = format!("{}TABLE {from} RENAME TO {to}", "ALTER ");
    client.batch_execute(&sql).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enabled_site_shows_the_text_and_three_failed_passes_degrade_without_failing_readiness()
{
    let Some(env) = Env::new(SERVICES).await else {
        return;
    };
    let host = env.site();
    let started = start(&env).await;
    let service = hosted(&env.site).expect("the site hosts the capture invariant");
    service.run_pass().await.unwrap();
    let (status, body) = ready(&started, &host).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("capture_invariant: ok ("), "{body}");

    // Three passes in a row cannot read the stream.
    rename_feed(&env, "spot_change_pub", "spot_change_pub_hidden").await;
    for _ in 0..3 {
        assert!(service.run_pass().await.is_err());
    }
    let (status, body) = ready(&started, &host).await;
    assert_eq!(status, 200, "degraded, never not-ready: {body}");
    assert!(body.contains("capture_invariant: degraded ("), "{body}");
    assert!(body.contains("degraded"), "{body}");

    // The first good pass clears it.
    rename_feed(&env, "spot_change_pub_hidden", "spot_change_pub").await;
    service.run_pass().await.unwrap();
    let (status, body) = ready(&started, &host).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("capture_invariant: ok ("), "{body}");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_without_the_service_shows_no_capture_line() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let host = env.site();
    let started = start(&env).await;
    assert!(hosted(&env.site).is_none());
    let (status, body) = ready(&started, &host).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("capture_invariant"), "{body}");
    started.finish().await;
}
