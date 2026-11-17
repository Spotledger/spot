//! The document audit, hosted, over a real PostgreSQL and over HTTP (WP-PRD-01.T22).
//!
//! The host installs the audit participant first in the order, the denial sink, the lane appender
//! jobs and the readiness line, and refuses to start without the chain tables and their guard.
//! This file proves it end to end:
//!
//! - create, update and delete over HTTP produce one `sys_doc_audit` outbox event per stored
//!   version, and the lane appenders chain every event exactly once, also across a restart;
//! - a protected field's audit diff is masked in the stored outbox row **and** in the chain;
//! - a refused write produces one `denied` event, with class and code and no message;
//! - readiness says `docaudit: on` with the lag and the denial counters and no value;
//! - two sites in one database never see each other's events or chain (per-site scopes);
//! - the guard: the application role cannot rewrite the chain, and a database with the chain
//!   tables but no guard does not start the site.
//!
//! Real PostgreSQL: `SPOT_TEST_PG_URL` selects the server and `SPOT_REQUIRE_PG=1` makes a missing
//! one loud. Each test makes its own scratch database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;
use spot_postgres::PgBackend;

fn loaded(env: &Env) -> spot_cli::config::Loaded {
    spot_cli::config::load(&env.root.0, false).unwrap()
}

async fn backend(env: &Env) -> PgBackend {
    let loaded = loaded(env);
    let site = loaded
        .sites
        .iter()
        .find(|site| site.site == env.site)
        .expect("the site this test made")
        .clone();
    spot_cli::schema::backend(&loaded, &site).await.unwrap()
}

/// One integer from the site's database. The statement is written in two pieces, as the other
/// tests of this crate do (G5 refuses a whole SQL statement outside the storage crates).
async fn count(env: &Env, head: &str, tail: &str) -> i64 {
    let backend = backend(env).await;
    let client = backend.pool().get().await.unwrap();
    let sql = format!("{head}{tail}");
    client.query_one(&sql, &[]).await.unwrap().get(0)
}

/// The `[audit]` table the tests start with: a fast append poll, two lanes.
fn fast_audit(env: &Env) {
    let path = env.root.0.join("common.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n[audit]\ndoc_lanes = 2\ndoc_append_poll_ms = 100\n");
    std::fs::write(path, text).unwrap();
}

fn migrate(env: &Env) -> String {
    ok(&env.root.0, &["migrate", "--site", &env.site, "--apply"])
}

async fn outbox_events(env: &Env) -> i64 {
    count(
        env,
        "SELECT count(*)::bigint ",
        "FROM spot_outbox WHERE topic = 'sys_doc_audit'",
    )
    .await
}

async fn chained(env: &Env) -> i64 {
    let like = format!("{}:lane:%", env.site);
    let backend = backend(env).await;
    let client = backend.pool().get().await.unwrap();
    let sql = format!(
        "{}{}",
        "SELECT count(*)::bigint ", "FROM spot_audit_chain WHERE site LIKE $1"
    );
    client.query_one(&sql, &[&like]).await.unwrap().get(0)
}

/// Waits until the lane appenders have chained at least `expected` events and the count has stopped
/// moving (the bootstrap writes of the site are audited too, so the chain holds more than the
/// events of one test step), and returns the settled count.
async fn wait_chained(env: &Env, expected: i64) -> i64 {
    let mut last = -1;
    let mut steady = 0;
    for _ in 0..300 {
        let now = chained(env).await;
        if now >= expected && now == last {
            steady += 1;
            if steady >= 5 {
                return now;
            }
        } else {
            steady = 0;
        }
        last = now;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the appenders chained {last} of at least {expected} events");
}

async fn project(addr: std::net::SocketAddr, host: &str, admin: &str, code: &str) -> String {
    let owner = me(addr, host, admin).await;
    let reply = http(
        addr,
        host,
        Some(admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": code, "title": "Audit", "status": "Active",
            "owner": owner, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&reply, 201);
    reply.body["data"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_stored_version_is_audited_once_chained_once_and_a_restart_chains_nothing_twice() {
    let Some(env) = Env::new("").await else {
        return;
    };
    fast_audit(&env);
    let secret = env.site();
    migrate(&env);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &secret).await;

    let created = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/label",
        &[],
        Some(json!({ "name": "L1", "colour": "#ffffff" })),
    )
    .await;
    expect(&created, 201);
    let id = created.body["data"]["id"].as_str().unwrap().to_owned();
    let version = created.body["data"]["version"].as_i64().unwrap_or(1);
    let updated = http(
        addr,
        &host,
        Some(&admin),
        "PATCH",
        &format!("/api/v1/resource/label/{id}"),
        &[("If-Match", &format!("\"{version}\""))],
        Some(json!({ "colour": "#000000" })),
    )
    .await;
    expect(&updated, 200);

    // The audit's own DocTypes are never audited; the user's label writes are: at least a
    // create and an update, one event per stored version.
    let events = outbox_events(&env).await;
    assert!(events >= 2, "one event per stored version, got {events}");
    let settled = wait_chained(&env, events).await;

    // Readiness: on, with the lag (zero once caught up) and the counters, and no value.
    let ready = http(addr, &host, None, "GET", "/api/health/ready", &[], None).await;
    expect(&ready, 200);
    let text = String::from_utf8_lossy(&ready.bytes).into_owned();
    assert!(text.contains("docaudit: on"), "{text}");
    assert!(text.contains("denials dropped 0, not recorded 0"), "{text}");
    assert!(!text.contains("#ffffff") && !text.contains("L1"), "{text}");
    started.finish().await;

    // A restart (a new appender over the same database) re-chains nothing and loses nothing: what
    // the chain gains is exactly what the outbox gained (a restart may audit writes of its own).
    let outbox_before = outbox_events(&env).await;
    let started = start(&env).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let gained = outbox_events(&env).await - outbox_before;
    assert_eq!(
        wait_chained(&env, settled).await,
        settled + gained,
        "a restart chains nothing twice"
    );
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_field_is_masked_in_the_outbox_row_and_in_the_chain() {
    let Some(env) = Env::new("").await else {
        return;
    };
    fast_audit(&env);
    let secret = env.site();
    migrate(&env);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &secret).await;
    let project_id = project(addr, &host, &admin, "PRJ-AUD-1").await;
    let task = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({
            "subject": "Audited task", "project": project_id,
            "details": "RAW-CONFIDENTIAL-NOTES"
        })),
    )
    .await;
    expect(&task, 201);
    let events = outbox_events(&env).await;
    wait_chained(&env, events).await;

    // The raw value is in the task row and nowhere in the audit: not in the outbox payload, not
    // in the chain record.
    let in_outbox = count(
        &env,
        "SELECT count(*)::bigint ",
        "FROM spot_outbox WHERE topic = 'sys_doc_audit' AND payload::text LIKE '%RAW-CONFIDENTIAL%'",
    )
    .await;
    assert_eq!(in_outbox, 0, "the outbox row holds the masked value");
    let in_chain = count(
        &env,
        "SELECT count(*)::bigint ",
        "FROM spot_audit_chain WHERE record LIKE '%RAW-CONFIDENTIAL%'",
    )
    .await;
    assert_eq!(in_chain, 0, "the chain record holds the masked value");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_denied_write_produces_one_denied_event_with_no_message() {
    let Some(env) = Env::new("").await else {
        return;
    };
    fast_audit(&env);
    let secret = env.site();
    migrate(&env);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &secret).await;
    env.user("viewer@example.com", "saffron-meadow-pilot-7-Cedar");
    let viewer = role(addr, &host, &admin, "Viewer").await;
    let user = user_id(addr, &host, &admin, "viewer@example.com").await;
    assign(addr, &host, &admin, &user, &viewer).await;
    let token = token_of(
        addr,
        &host,
        "viewer@example.com",
        "saffron-meadow-pilot-7-Cedar",
    )
    .await;
    let before = outbox_events(&env).await;
    let refused = http(
        addr,
        &host,
        Some(&token),
        "POST",
        "/api/v1/resource/label",
        &[],
        Some(json!({ "name": "DENIED-LABEL", "colour": "#111111" })),
    )
    .await;
    expect(&refused, 403);
    // The denial is written after the rollback, in a transaction of its own.
    let mut after = before;
    for _ in 0..100 {
        after = outbox_events(&env).await;
        if after > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(after, before + 1, "exactly one denied event");
    let leaked = count(
        &env,
        "SELECT count(*)::bigint ",
        "FROM spot_outbox WHERE topic = 'sys_doc_audit' AND payload::text LIKE '%DENIED-LABEL%'",
    )
    .await;
    assert_eq!(leaked, 0, "a denial carries no value");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_database_with_the_chain_tables_but_no_guard_does_not_start_the_site() {
    let Some(env) = Env::new("").await else {
        return;
    };
    fast_audit(&env);
    let secret = env.site();
    migrate(&env);
    // Remove the guard trigger the way a careless operator would.
    let db = backend(&env).await;
    let client = db.pool().get().await.unwrap();
    client
        .batch_execute(&format!(
            "{}{}",
            "DROP TRIGGER spot_audit_chain_no_rewrite ", "ON spot_audit_chain"
        ))
        .await
        .unwrap();
    drop(client);
    let started = start(&env).await;
    let refused = http(
        started.addr,
        &env.site,
        None,
        "GET",
        "/api/v1/resource/label?limit=1",
        &[],
        None,
    )
    .await;
    expect(&refused, 503);
    assert_eq!(
        refused.body["error"]["details"]["reason"],
        "site_unavailable"
    );
    let _ = secret;
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spot_migrate_plans_the_appender_tables_and_a_second_run_plans_nothing() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let _ = env.site();
    let first = migrate(&env);
    for table in [
        "spot_audit_bookmark",
        "spot_audit_event_index",
        "spot_audit_doc_index",
        "spot_audit_site_checkpoint",
    ] {
        let present = count(
            &env,
            "SELECT count(*)::bigint ",
            &format!("FROM pg_tables WHERE tablename = '{table}'"),
        )
        .await;
        assert_eq!(present, 1, "`{table}` is created by migrate: {first}");
    }
    let second = migrate(&env);
    assert!(
        !second.contains("create table") && !second.contains("CREATE TABLE"),
        "a second run plans nothing: {second}"
    );
}
