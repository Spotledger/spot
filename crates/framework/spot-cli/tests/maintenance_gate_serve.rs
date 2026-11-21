//! `spot serve` honours the maintenance gate (WP-MIG-07.T5). Real PostgreSQL through
//! `SPOT_TEST_PG_URL` (skipped with a printed reason when unset, as the other host tests do).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, dead_code)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Env, Started, http, login};
use spot_cli::services::maintenance_gate::{Settings, exempt, retry_after_secs};
use spot_core::{SiteId, SystemClock};
use spot_migration_apply::gate::AdmissionGate;
use spot_migration_apply_pg::{PgAdmissionGate, PgApplySchema};
use spot_postgres::PgBackend;

const SERVICES: &str = "[services.maintenance_gate]\nrefresh_ms = 100\nttl_ms = 100\n";

async fn pool_of(env: &Env) -> deadpool_postgres::Pool {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    let pg = loaded.admin.clone().with_dbname(&site.database);
    PgBackend::connect(pg).await.unwrap().pool()
}

fn control(pool: deadpool_postgres::Pool, env: &Env) -> PgAdmissionGate {
    PgAdmissionGate::new(
        pool,
        SiteId::new(&env.site).unwrap(),
        Arc::new(SystemClock::new()),
        Duration::from_millis(100),
    )
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros(),
    )
    .unwrap()
}

/// A request that normally gets the ordinary sign-in answer (401) and never needs a session.
async fn probe(started: &Started, env: &Env) -> (u16, String) {
    let reply = login(started.addr, &env.site, "administrator", "wrong").await;
    (
        reply.status,
        String::from_utf8_lossy(&reply.bytes).into_owned(),
    )
}

/// Polls until `probe` answers `want` (the refresh interval is 100 ms), within three seconds.
async fn until(
    started: &Started,
    env: &Env,
    want: impl Fn(u16) -> bool,
    what: &str,
) -> (u16, String) {
    for _ in 0..30 {
        let (status, body) = probe(started, env).await;
        if want(status) {
            return (status, body);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("maintenance gate: {what}");
}

#[test]
fn settings_and_exemptions() {
    assert!(Settings::default().validate().is_ok());
    for bad in [
        Settings {
            refresh_ms: 99,
            ..Settings::default()
        },
        Settings {
            refresh_ms: 30_001,
            ..Settings::default()
        },
        Settings {
            refresh_ms: 1_000,
            ttl_ms: 999,
            ..Settings::default()
        },
    ] {
        assert!(
            bad.validate()
                .unwrap_err()
                .to_string()
                .contains("maintenance_gate")
        );
    }
    assert_eq!(retry_after_secs(), 30);
    let get = axum::http::Method::GET;
    for path in ["/api/health/live", "/api/health/ready", "/metrics"] {
        assert!(exempt(&get, path));
        assert!(!exempt(&axum::http::Method::POST, path));
    }
    for path in [
        "/api/health/ready/",
        "/api/v1/auth/login",
        "/metricsx",
        "/api/health",
    ] {
        assert!(!exempt(&get, path), "{path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_gate_refuses_requests_and_opening_it_restores_them_without_a_restart() {
    let Some(env) = Env::new(SERVICES).await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    let started = common::start(&env).await;
    let (status, _) = probe(&started, &env).await;
    assert_eq!(
        status, 401,
        "an open gate passes the request to its handler"
    );

    let gate = control(pool_of(&env).await, &env);
    gate.close("schema change for the test", now())
        .await
        .unwrap();
    let (status, body) = until(
        &started,
        &env,
        |s| s == 503,
        "the closed gate was never honoured",
    )
    .await;
    assert_eq!(status, 503);
    assert!(body.contains("server.maintenance"), "{body}");
    // Retry-After and the request id.
    let reply = login(started.addr, &env.site, "administrator", "wrong").await;
    assert_eq!(reply.header("retry-after"), Some("30"));
    assert!(
        !reply.body["error"]["request_id"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    );
    // No SQL, table name, connection string or path in the refusal.
    for leak in [
        "SELECT",
        "spot_migration_control",
        env.url.as_str(),
        "postgres://",
        "/tmp",
    ] {
        assert!(!body.contains(leak), "the refusal leaked `{leak}`: {body}");
    }
    // Probes still answer.
    let ready = http(
        started.addr,
        &env.site,
        None,
        "GET",
        "/api/health/ready",
        &[],
        None,
    )
    .await;
    assert!(!String::from_utf8_lossy(&ready.bytes).contains("server.maintenance"));
    let live = http(
        started.addr,
        &env.site,
        None,
        "GET",
        "/api/health/live",
        &[],
        None,
    )
    .await;
    assert_eq!(live.status, 200);

    gate.open(now()).await.unwrap();
    let (status, _) = until(
        &started,
        &env,
        |s| s != 503,
        "the opened gate was never honoured",
    )
    .await;
    assert_eq!(status, 401);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_gate_is_closed_and_recovers_but_an_absent_schema_is_open() {
    let Some(env) = Env::new(SERVICES).await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    let started = common::start(&env).await;
    let pool = pool_of(&env).await;
    let client = pool.get().await.unwrap();

    // Present but unreadable (a column renamed): closed.
    client
        .batch_execute("ALTER TABLE spot_migration_control.gate RENAME COLUMN state TO state_x")
        .await
        .unwrap();
    until(
        &started,
        &env,
        |s| s == 503,
        "an unreadable gate did not close the site",
    )
    .await;
    client
        .batch_execute("ALTER TABLE spot_migration_control.gate RENAME COLUMN state_x TO state")
        .await
        .unwrap();
    until(
        &started,
        &env,
        |s| s != 503,
        "a recovered gate stayed closed",
    )
    .await;

    // No control schema at all (a site older than WP-MIG-07.T4): open.
    client
        .batch_execute("DROP SCHEMA spot_migration_control CASCADE")
        .await
        .unwrap();
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (status, _) = probe(&started, &env).await;
        assert_ne!(status, 503, "a site that never migrated is served");
    }
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    started.finish().await;
}
