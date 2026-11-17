//! WP-PRD-01.T43: when the database cannot hand out a connection, a sign-in answers HTTP 503
//! `server.unavailable`, retryable, with a `retry_after` detail (never a non-retryable 500), and
//! it answers the same for a known and an unknown user name.
//!
//! The server runs on real PostgreSQL with a realistic pool (`postgres.pool_max_size = 8` (the test default)) and a
//! short `postgres.pool_wait_ms`. The test takes a lock on every table of the site's database from
//! a separate connection and starts more sign-ins than the pool has connections: each one that gets
//! a connection waits on the lock and keeps it, so the pool is full. A sign-in then waits out the
//! pool wait and is refused with a retryable 503 and `Retry-After`, the same for a known and an
//! unknown name, while a request that needs no connection keeps being served. After the lock is
//! released the same sign-in succeeds within a bounded wait.
//! Skipped, with a printed reason, when `SPOT_TEST_PG_URL` is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_cli::Options;
use spot_login::LoginConfig;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";

struct Reply {
    status: u16,
    body: Json,
    raw: String,
}

async fn http(addr: SocketAddr, host: &str, token: Option<&str>, path: &str, body: Json) -> Reply {
    let body = body.to_string();
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    head.push_str(&format!("\r\n{body}"));
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let raw = String::from_utf8(bytes).unwrap();
    let (headers, payload) = raw.split_once("\r\n\r\n").unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        body: serde_json::from_str(payload).unwrap_or(Json::Null),
        raw,
    }
}

async fn login(addr: SocketAddr, host: &str, user: &str, password: &str) -> Reply {
    let body = json!({ "user": user, "password": password });
    http(addr, host, None, "/api/v1/auth/login", body).await
}

/// Every table of the site's database, locked exclusively in the transaction this client opens.
const LOCK_EVERYTHING: &str = "BEGIN; DO $$ DECLARE t text; BEGIN FOR t IN SELECT quote_ident(tablename) FROM pg_tables WHERE schemaname = current_schema() LOOP EXECUTE 'LOCK TABLE ' || t || ' IN ACCESS EXCLUSIVE MODE'; END LOOP; END $$;";

/// How many sessions of this database wait for a lock right now.
const WAITING_ON_A_LOCK: &str = "SELECT count(*)::bigint FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'";

fn assert_unavailable(reply: &Reply, what: &str) {
    assert_eq!(reply.status, 503, "{what}: {}", reply.raw);
    assert_eq!(reply.body["error"]["code"], "server.unavailable", "{what}");
    assert_eq!(reply.body["error"]["retryable"], true, "{what}");
    assert!(
        reply.body["error"]["details"]["retry_after"]
            .as_u64()
            .is_some_and(|secs| secs > 0),
        "{what}: a positive retry_after: {}",
        reply.raw
    );
    assert!(reply.body["error"]["request_id"].is_string(), "{what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sign_in_without_a_connection_is_a_retryable_503_for_known_and_unknown_names_alike() {
    let Some(env) = Env::new().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let output = env.ok(&["new-site", &env.site]);
    let secret = output.trim().rsplit(' ').next().unwrap().to_owned();
    // A pool that background services and requests share, and a short wait for a connection.
    let common = std::fs::read_to_string(env.root.join("common.toml")).unwrap();
    std::fs::write(
        env.root.join("common.toml"),
        common.replace("[postgres]\n", "[postgres]\npool_wait_ms = 1000\n"),
    )
    .unwrap();

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let options = Options {
        clock: None,
        login: LoginConfig::for_tests(),
    };
    let server = spot_cli::start_with(&env.root, false, options, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let host = env.site.clone();

    // The connection is free: the one-time secret signs in once and is replaced by a password
    // (the same steps as the password-throttle test), and the password signs in.
    let first = login(addr, &host, "administrator", &secret).await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let token = first.body["data"]["token"].as_str().unwrap().to_owned();
    let change = json!({ "current": secret, "new": PASSWORD });
    let changed = http(addr, &host, Some(&token), "/api/v1/auth/password", change).await;
    assert!(matches!(changed.status, 200 | 204), "{}", changed.raw);
    let secret = PASSWORD.to_owned();
    let signed_in = login(addr, &host, "administrator", &secret).await;
    assert_eq!(signed_in.status, 200, "{}", signed_in.raw);

    // Lock every table from a separate connection, then start more sign-ins than the pool has
    // connections: each one that gets a connection blocks on the lock and holds it.
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    let own = spot_cli::schema::backend(&loaded, site).await.unwrap();
    let client = own.pool().get().await.unwrap();
    client.batch_execute(LOCK_EVERYTHING).await.unwrap();
    let mut blockers = Vec::new();
    for _ in 0..24 {
        let (host, secret) = (host.clone(), secret.clone());
        blockers.push(tokio::spawn(async move {
            login(addr, &host, "administrator", &secret).await
        }));
    }
    // Wait for the condition, not a sleep: sessions of the site waiting on the lock, and the
    // blockers that did not get a connection already refused.
    let started = Instant::now();
    loop {
        let waiting: i64 = client
            .query_one(WAITING_ON_A_LOCK, &[])
            .await
            .unwrap()
            .get(0);
        if waiting >= 3 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the sign-ins never reached the locked tables"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Past the pool wait every blocker that found no connection has been refused; the rest hold
    // the pool.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // The pool has nothing to give: both answers are the same retryable 503 with Retry-After, for
    // the valid user and for a name that does not exist.
    let known = login(addr, &host, "administrator", &secret).await;
    assert_unavailable(&known, "a known user");
    assert!(
        known.raw.to_ascii_lowercase().contains("\r\nretry-after:"),
        "a Retry-After header: {}",
        known.raw
    );
    let unknown = login(addr, &host, "nobody-by-this-name", "whatever-Violet-77").await;
    assert_unavailable(&unknown, "an unknown user");
    assert_eq!(known.status, unknown.status);
    assert_eq!(
        known.body["error"]["code"], unknown.body["error"]["code"],
        "the answer does not tell a known name from an unknown one"
    );
    let keys = |reply: &Reply| {
        let mut keys: Vec<String> = reply.body["error"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(keys(&known), keys(&unknown));
    assert_eq!(
        known.body["error"]["message"],
        unknown.body["error"]["message"]
    );
    // A request that needs no connection keeps being served.
    let live = tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head =
            format!("GET /api/health/live HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        String::from_utf8(bytes).unwrap()
    })
    .await
    .expect("the liveness probe answers while the pool is full");
    assert!(live.starts_with("HTTP/1.1 200"), "{live}");

    // Release the lock: the blocked sign-ins finish, and the same sign-in succeeds soon.
    client.batch_execute("ROLLBACK").await.unwrap();
    drop(client);
    for blocker in blockers {
        let reply = blocker.await.unwrap();
        assert!(
            reply.status == 200 || reply.status == 503,
            "a blocked sign-in ends as success or the retryable refusal: {}",
            reply.raw
        );
    }
    let recovered = Instant::now();
    let mut again = login(addr, &host, "administrator", &secret).await;
    while again.status != 200 && recovered.elapsed() < Duration::from_secs(20) {
        assert_eq!(again.status, 503, "{}", again.raw);
        tokio::time::sleep(Duration::from_millis(250)).await;
        again = login(addr, &host, "administrator", &secret).await;
    }
    assert_eq!(again.status, 200, "no recovery within 20 s: {}", again.raw);

    stop.send(()).unwrap();
    server.join().await.unwrap();
}
