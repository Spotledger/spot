//! WP-PRD-01.T13: `GET /metrics` is an operator surface. With `require_auth = true` it is served
//! only to a Tenant Administrator of the site the request resolved to; any other signed-in user
//! gets the refusal an unauthorised API call gets, with no counter text. Real PostgreSQL, real
//! HTTP.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::{Value as Json, json};
use spot_cli::Options;
use spot_login::LoginConfig;
use std::net::SocketAddr;
use std::time::Duration;
use support::{Env, run, text};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ADMIN_PASSWORD: &str = "violet-lantern-staple-9-Quartz";
const PLAIN_PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";
/// A name every rendering of the counters contains once any request has been counted.
const METRIC_TEXT: &str = "spot_";

struct Reply {
    status: u16,
    body: Json,
    raw: String,
}

async fn request(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Json>,
) -> Reply {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    head.push_str(&format!("\r\n{body}"));
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes))
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

fn secret_of(output: &str) -> String {
    output.trim().rsplit(' ').next().unwrap().to_owned()
}

fn options() -> Options {
    Options {
        clock: None,
        login: LoginConfig::for_tests(),
    }
}

async fn login(addr: SocketAddr, host: &str, user: &str, password: &str) -> Reply {
    request(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": user, "password": password})),
    )
    .await
}

fn token_in(reply: &Reply) -> String {
    reply.body["data"]["token"].as_str().unwrap().to_owned()
}

/// The Administrator's full session: the one-time secret only chooses a password.
async fn administrator(addr: SocketAddr, host: &str, secret: &str) -> String {
    let first = login(addr, host, "administrator", secret).await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let changed = request(
        addr,
        host,
        Some(&token_in(&first)),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": secret, "new": ADMIN_PASSWORD})),
    )
    .await;
    assert_eq!(changed.status, 200, "{}", changed.raw);
    token_in(&changed)
}

/// A user with a password, made through the CLI before the server starts.
fn add_user(env: &Env, site: &str, email: &str) {
    env.ok(&["add-user", site, email]);
    let out = run(
        &env.root,
        &["set-password", site, email, "--password-stdin"],
        Some(&format!("{PLAIN_PASSWORD}\n")),
    );
    assert!(out.status.success(), "{}", text(&out));
}

async fn start(
    env: &Env,
) -> (
    spot_cli::Running,
    SocketAddr,
    tokio::sync::oneshot::Sender<()>,
) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = spot_cli::start_with(&env.root, false, options(), async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

fn assert_refused_without_counters(reply: &Reply, status: u16) {
    assert_eq!(reply.status, status, "{}", reply.raw);
    assert!(!reply.raw.contains(METRIC_TEXT), "{}", reply.raw);
    assert!(!reply.raw.contains("# TYPE"), "{}", reply.raw);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_an_administrator_of_the_resolved_site_reads_metrics() {
    let Some(env) = Env::new().await else { return };
    let site_a = env.site.clone();
    let site_b = format!(
        "spot-test-b-{}",
        spot_storage_contract::RecordId::new_v7()
            .to_string()
            .replace('-', "")
    );
    let secret_a = secret_of(&env.ok(&["new-site", &site_a]));
    let secret_b = secret_of(&env.ok(&["new-site", &site_b]));
    add_user(&env, &site_a, "plain@example.com");
    let (server, addr, stop) = start(&env).await;

    let admin_a = administrator(addr, &site_a, &secret_a).await;
    let admin_b = administrator(addr, &site_b, &secret_b).await;
    let plain = token_in(&login(addr, &site_a, "plain@example.com", PLAIN_PASSWORD).await);
    // Some traffic, so the counters are not empty.
    let _ = request(
        addr,
        &site_a,
        Some(&plain),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;

    // An ordinary user: 403, the standard refusal, no counter text.
    let denied = request(addr, &site_a, Some(&plain), "GET", "/metrics", None).await;
    assert_refused_without_counters(&denied, 403);
    assert_eq!(
        denied.body["error"]["code"], "permission.denied",
        "{}",
        denied.raw
    );
    assert!(
        denied.body["error"]["request_id"].is_string(),
        "{}",
        denied.raw
    );
    assert!(denied.body.get("request_id").is_none(), "{}", denied.raw);
    // The same body as an API call the user is not allowed to make.
    let api = request(
        addr,
        &site_a,
        Some(&plain),
        "GET",
        "/api/v1/resource/role_assignment?limit=1",
        None,
    )
    .await;
    if api.status == 403 {
        assert_eq!(api.body["error"]["code"], denied.body["error"]["code"]);
        assert_eq!(
            api.body["error"]["message"],
            denied.body["error"]["message"]
        );
    }

    // An administrator reads it.
    let served = request(addr, &site_a, Some(&admin_a), "GET", "/metrics", None).await;
    assert_eq!(served.status, 200, "{}", served.raw);
    assert!(served.raw.contains(METRIC_TEXT), "{}", served.raw);

    // Site B's administrator, reading through site A's host name: refused, and no counters.
    let crossed = request(addr, &site_a, Some(&admin_b), "GET", "/metrics", None).await;
    assert!(
        crossed.status == 401 || crossed.status == 403,
        "{}",
        crossed.raw
    );
    assert_refused_without_counters(&crossed, crossed.status);
    // ...and it reads site B's own metrics through site B's host name.
    let own = request(addr, &site_b, Some(&admin_b), "GET", "/metrics", None).await;
    assert_eq!(own.status, 200, "{}", own.raw);

    // A user of site A presenting its token to site B's host name is not a user there.
    let forged = request(addr, &site_b, Some(&plain), "GET", "/metrics", None).await;
    assert_refused_without_counters(&forged, 401);

    // No credential at all keeps the existing 401.
    let anonymous = request(addr, &site_a, None, "GET", "/metrics", None).await;
    assert_refused_without_counters(&anonymous, 401);

    // The refusals were counted: the administrator now sees a 403 on the metrics route.
    let after = request(addr, &site_a, Some(&admin_a), "GET", "/metrics", None).await;
    assert!(after.raw.contains("403"), "{}", after.raw);

    let _ = stop.send(());
    server.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restricted_first_sign_in_session_is_refused_by_the_password_gate() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let (server, addr, stop) = start(&env).await;
    // The Administrator on the one-time secret holds the role, but may only choose a password.
    let first = login(addr, &env.site, "administrator", &secret).await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let restricted = request(
        addr,
        &env.site,
        Some(&token_in(&first)),
        "GET",
        "/metrics",
        None,
    )
    .await;
    assert_refused_without_counters(&restricted, 403);
    assert_eq!(
        restricted.body["error"]["code"], "cli.password_change_required",
        "{}",
        restricted.raw
    );
    let _ = stop.send(());
    server.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_require_auth_a_loopback_collector_scrapes_with_no_credential() {
    let Some(env) = Env::new().await else { return };
    env.ok(&["new-site", &env.site]);
    let path = env.root.join("common.toml");
    let mut common = std::fs::read_to_string(&path).unwrap();
    common.push_str("\n[services.metrics]\nrequire_auth = false\n");
    std::fs::write(path, common).unwrap();
    let (server, addr, stop) = start(&env).await;
    // Warm the counters with one counted request.
    let _ = request(addr, &env.site, None, "GET", "/api/health/live", None).await;
    let open = request(addr, &env.site, None, "GET", "/metrics", None).await;
    assert_eq!(open.status, 200, "{}", open.raw);
    let _ = stop.send(());
    server.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_does_not_start_with_open_metrics_on_a_non_loopback_bind() {
    let Some(env) = Env::new().await else { return };
    env.ok(&["new-site", &env.site]);
    let path = env.root.join("common.toml");
    let common = std::fs::read_to_string(&path)
        .unwrap()
        .replace("127.0.0.1:0", "0.0.0.0:0");
    std::fs::write(
        &path,
        format!("{common}\n[services.metrics]\nrequire_auth = false\n"),
    )
    .unwrap();
    let (_stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let started = spot_cli::start_with(&env.root, false, options(), async {
        let _ = stopped.await;
    })
    .await;
    let error = started
        .err()
        .expect("the host refuses to start")
        .to_string();
    assert!(error.contains("services.metrics.require_auth"), "{error}");
    assert!(error.contains("loopback"), "{error}");
}
