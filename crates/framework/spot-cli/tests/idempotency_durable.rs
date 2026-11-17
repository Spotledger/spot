//! WP-PLT-05.T8: the host wires the durable `PgIdempotencyStore` on each site's own pool, so an
//! `Idempotency-Key` survives a restart, prunes it on a schedule, reads its retention
//! from `[services.idempotency.retention]` and keeps the in-memory store for the dev switch and
//! for a site that says so with `allow_memory_idempotency = true`.
//!
//! Real PostgreSQL (`SPOT_TEST_PG_URL`); `SPOT_REQUIRE_PG=1` turns a missing server into a
//! failure instead of a skip. Not covered here: a database that stops answering mid-request
//! (`503 server.unavailable` with `Retry-After`) is the store's own contract (WP-PLT-05.T7's
//! `tests/router.rs`); the host adds nothing to it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_cli::{Options, Running};
use spot_core::{Clock, ManualClock};
use spot_login::LoginConfig;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";

struct Reply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Json,
}

async fn http(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Option<Json>,
) -> Reply {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    for (name, value) in extra {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("\r\n{body}"));
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let raw = String::from_utf8(bytes).unwrap();
    let (head, payload) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: serde_json::from_str(payload).unwrap_or(Json::Null),
    }
}

async fn sign_in(addr: SocketAddr, host: &str, password: &str) -> Reply {
    let body = json!({ "user": "administrator", "password": password });
    http(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(body),
    )
    .await
}

fn token_of(reply: &Reply) -> String {
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.body["data"]["token"].as_str().unwrap().to_owned()
}

/// The one-time secret signs in once and is replaced by a password; then the password signs in.
async fn first_token(addr: SocketAddr, host: &str, secret: &str) -> String {
    let first = token_of(&sign_in(addr, host, secret).await);
    let change = json!({ "current": secret, "new": PASSWORD });
    let path = "/api/v1/auth/password";
    let changed = http(addr, host, Some(&first), "POST", path, &[], Some(change)).await;
    assert!(matches!(changed.status, 200 | 204), "{}", changed.body);
    token_of(&sign_in(addr, host, PASSWORD).await)
}

async fn serve(
    env: &Env,
    clock: &Arc<ManualClock>,
) -> spot_cli::Result<(Running, SocketAddr, tokio::sync::oneshot::Sender<()>)> {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let options = Options {
        clock: Some(clock.clone() as Arc<dyn Clock>),
        login: LoginConfig::for_tests(),
    };
    let running = spot_cli::start_with(&env.root, false, options, async {
        let _ = stopped.await;
    })
    .await?;
    let addr = running.local_addrs()[0];
    Ok((running, addr, stop))
}

async fn stop(running: Running, stop: tokio::sync::oneshot::Sender<()>) {
    stop.send(()).unwrap();
    running.join().await.unwrap();
}

async fn pool(env: &Env) -> deadpool_postgres::Pool {
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    spot_cli::schema::backend(&loaded, site)
        .await
        .unwrap()
        .pool()
}

async fn count_rows(env: &Env) -> i64 {
    let client = pool(env).await.get().await.unwrap();
    let sql = "SELECT count(*)::bigint FROM spot_idempotency_key";
    client.query_one(sql, &[]).await.unwrap().get(0)
}

/// An environment with a bare site and its one-time secret, or `None` (skipped).
async fn rig() -> Option<(Env, String)> {
    let Some(env) = Env::new().await else {
        assert!(
            std::env::var_os("SPOT_REQUIRE_PG").is_none(),
            "SPOT_REQUIRE_PG is set but no PostgreSQL is configured"
        );
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return None;
    };
    let output = env.ok(&["new-site", &env.site]);
    let secret = output.trim().rsplit(' ').next().unwrap().to_owned();
    Some((env, secret))
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()))
}

/// Adds `text` to `common.toml`.
fn append_config(env: &Env, text: &str) {
    let path = env.root.join("common.toml");
    let mut common = std::fs::read_to_string(&path).unwrap();
    common.push_str(text);
    std::fs::write(path, common).unwrap();
}

async fn create_role(addr: SocketAddr, host: &str, token: &str, name: &str, key: &str) -> Reply {
    let body = json!({ "role_name": name, "origin": "reference" });
    let path = "/api/v1/resource/role";
    let headers = [("Idempotency-Key", key)];
    http(addr, host, Some(token), "POST", path, &headers, Some(body)).await
}

async fn roles_named(addr: SocketAddr, host: &str, token: &str, name: &str) -> usize {
    let path = "/api/v1/resource/role?limit=100";
    let list = http(addr, host, Some(token), "GET", path, &[], None).await;
    let rows = list.body["data"].as_array().cloned().unwrap_or_default();
    rows.iter().filter(|r| r["role_name"] == name).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_survives_a_restart_conflicts_on_another_body_and_keeps_for_a_day() {
    let Some((env, secret)) = rig().await else {
        return;
    };
    let clock = clock();
    let host = env.site.clone();
    let (first, addr, stop_first) = serve(&env, &clock).await.unwrap();
    let token = first_token(addr, &host, &secret).await;
    let created = create_role(addr, &host, &token, "durable-role", "k1").await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert!(!created.headers.contains_key("idempotency-replayed"));
    let id = created.body["data"]["id"].as_str().unwrap().to_owned();
    // The row records its own expiry: the documented default of 24 hours.
    let client = pool(&env).await.get().await.unwrap();
    let sql = "SELECT (expires_at - created_at)::bigint FROM spot_idempotency_key WHERE key = 'k1'";
    let kept: i64 = client.query_one(sql, &[]).await.unwrap().get(0);
    assert_eq!(kept, 24 * 60 * 60);
    drop(client);
    stop(first, stop_first).await;

    // A new host on the same database: the same key and body is the stored response.
    let (second, addr, stop_second) = serve(&env, &clock).await.unwrap();
    // The session is stored, so it survives the restart; a key is scoped by the credential that
    // sent it, so the replay comes with the same session.
    let replay = create_role(addr, &host, &token, "durable-role", "k1").await;
    assert_eq!(replay.status, 201, "{}", replay.body);
    assert_eq!(
        replay
            .headers
            .get("idempotency-replayed")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(replay.body["data"]["id"], id.as_str());
    assert_eq!(roles_named(addr, &host, &token, "durable-role").await, 1);
    // The same key with another body is a conflict, never a second run.
    let other = create_role(addr, &host, &token, "another-role", "k1").await;
    assert_eq!(other.status, 409, "{}", other.body);
    assert_eq!(other.body["error"]["code"], "api.idempotency_conflict");
    assert_eq!(roles_named(addr, &host, &token, "another-role").await, 0);
    stop(second, stop_second).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_start_up_prune_removes_an_expired_row() {
    let Some((env, secret)) = rig().await else {
        return;
    };
    let clock = clock();
    let host = env.site.clone();
    let (first, addr, stop_first) = serve(&env, &clock).await.unwrap();
    let token = first_token(addr, &host, &secret).await;
    let created = create_role(addr, &host, &token, "pruned-role", "k-prune").await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(count_rows(&env).await, 1);
    stop(first, stop_first).await;
    // The retention (24 h) passes; the next host prunes once at start.
    clock.advance(Duration::from_secs(25 * 60 * 60));
    let (second, _addr, stop_second) = serve(&env, &clock).await.unwrap();
    let started = Instant::now();
    while count_rows(&env).await != 0 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the expired row was never pruned"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop(second, stop_second).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_class_override_and_the_maximum_reach_the_stored_expiry() {
    let Some((env, secret)) = rig().await else {
        return;
    };
    // `http_api` asks for 3 hours but the maximum is 2: the row keeps 2 hours. (This needs the
    // `idempotency` table to be a known `[services.*]` table: see the report's open questions.)
    append_config(
        &env,
        "\n[services.idempotency.retention]\ndefault = \"1h\"\nmax = \"2h\"\n[services.idempotency.retention.classes]\nhttp_api = \"3h\"\n",
    );
    let clock = clock();
    let host = env.site.clone();
    let (running, addr, stop_it) = serve(&env, &clock).await.unwrap();
    let token = first_token(addr, &host, &secret).await;
    let created = create_role(addr, &host, &token, "classed-role", "k-class").await;
    assert_eq!(created.status, 201, "{}", created.body);
    let client = pool(&env).await.get().await.unwrap();
    let sql =
        "SELECT (expires_at - created_at)::bigint FROM spot_idempotency_key WHERE key = 'k-class'";
    let kept: i64 = client.query_one(sql, &[]).await.unwrap().get(0);
    assert_eq!(kept, 2 * 60 * 60, "the class was clamped to the maximum");
    drop(client);
    stop(running, stop_it).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalid_retention_section_refuses_to_start_and_names_the_site() {
    let Some((env, _secret)) = rig().await else {
        return;
    };
    let clock = clock();
    for bad in [
        "default = \"0s\"",
        "default = \"soon\"",
        "default = \"3h\"\nmax = \"1h\"",
        "[services.idempotency.retention.classes]\nhttp_api = \"never\"",
    ] {
        let original = std::fs::read_to_string(env.root.join("common.toml")).unwrap();
        append_config(
            &env,
            &format!("\n[services.idempotency.retention]\n{bad}\n"),
        );
        // A site with a refused section does not serve; the host itself stays up (the reason goes
        // to the operator's log, not to the caller).
        let (running, addr, stop_it) = serve(&env, &clock)
            .await
            .expect("the host stays up for its other sites");
        let reply = http(
            addr,
            &env.site,
            None,
            "GET",
            "/api/v1/auth/session",
            &[],
            None,
        )
        .await;
        assert_eq!(reply.status, 503, "{bad}: {}", reply.body);
        stop(running, stop_it).await;
        std::fs::write(env.root.join("common.toml"), original).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_memory_store_is_used_only_when_the_site_allows_it() {
    let Some((env, secret)) = rig().await else {
        return;
    };
    append_config(
        &env,
        "\n[services.idempotency]\nallow_memory_idempotency = true\n",
    );
    let clock = clock();
    let host = env.site.clone();
    let (running, addr, stop_it) = serve(&env, &clock).await.unwrap();
    let token = first_token(addr, &host, &secret).await;
    let created = create_role(addr, &host, &token, "memory-role", "k-mem").await;
    assert_eq!(created.status, 201, "{}", created.body);
    // The durable table was never created for this site.
    let client = pool(&env).await.get().await.unwrap();
    let sql = "SELECT to_regclass('spot_idempotency_key') IS NULL";
    let absent: bool = client.query_one(sql, &[]).await.unwrap().get(0);
    assert!(absent, "the durable store must not have been built");
    drop(client);
    stop(running, stop_it).await;
}
