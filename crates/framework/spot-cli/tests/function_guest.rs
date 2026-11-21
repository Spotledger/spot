//! A guest function over HTTP (WP-PLT-17.T3): a request with no credential at all is dispatched as
//! an anonymous caller and reaches only functions declared `guest`, over a read verb, under the
//! guest rate limit (one `anonymous` bucket shared by every anonymous caller). A request that
//! carries a credential that does not resolve is refused and never downgraded to a guest.
//!
//! Needs the router to serve these two operations without a bearer (see the report: the bearer
//! layer in `spot-http` must let `function_call` and `function_call_get` through unauthenticated).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_cli::Options;
use spot_cli::natives::{self, FunctionSpec};
use spot_core::ManualClock;
use spot_login::LoginConfig;
use support::Env;
use time::macros::datetime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut bytes))
        .await
        .expect("the response arrives")
        .expect("the whole response arrives");
    let raw = String::from_utf8(bytes).unwrap();
    let (headers, payload) = raw.split_once("\r\n\r\n").expect("an HTTP response");
    let status = headers
        .split_whitespace()
        .nth(1)
        .expect("a status line")
        .parse()
        .unwrap();
    Reply {
        status,
        body: serde_json::from_str(payload).unwrap_or(Json::Null),
        raw,
    }
}

const ADMIN_PASSWORD: &str = "violet-lantern-staple-9-Quartz";

/// The Administrator's session: the first-sign-in secret only lets them choose a password, so a
/// first call does that and later calls sign in with it.
async fn administrator(addr: SocketAddr, host: &str, secret: &str) -> String {
    let login = |password: &str| json!({"user": "administrator", "password": password});
    let chosen = request(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(login(ADMIN_PASSWORD)),
    )
    .await;
    if chosen.status == 200 {
        return chosen.body["data"]["token"].as_str().unwrap().to_owned();
    }
    let first = request(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(login(secret)),
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let first = first.body["data"]["token"].as_str().unwrap().to_owned();
    let changed = request(
        addr,
        host,
        Some(&first),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": secret, "new": ADMIN_PASSWORD})),
    )
    .await;
    assert_eq!(changed.status, 200, "{}", changed.raw);
    changed.body["data"]["token"].as_str().unwrap().to_owned()
}

fn text_of(out: &str) -> String {
    out.trim().rsplit(' ').next().unwrap().to_owned()
}

/// Counts the calls that reached a guest handler.
static RUNS: AtomicUsize = AtomicUsize::new(0);

const BASE: &str = "/api/v1/method/sample/guests";

fn register_handlers() {
    natives::register_function_fn(
        "sample",
        "guests",
        "hello",
        FunctionSpec::default(),
        |call| {
            Box::pin(async move {
                RUNS.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"hello": true, "signed_in": call.user.is_some()}))
            })
        },
    );
    natives::register_function_fn("sample", "guests", "write", FunctionSpec::default(), |_| {
        Box::pin(async {
            RUNS.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"wrote": true}))
        })
    });
    natives::register_function_fn(
        "sample",
        "guests",
        "limited",
        FunctionSpec {
            limit: Some((2, Duration::from_secs(60))),
            arguments: None,
        },
        |_| {
            Box::pin(async {
                RUNS.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"ok": true}))
            })
        },
    );
    natives::register_function_fn(
        "sample",
        "guests",
        "staff_only",
        FunctionSpec::default(),
        |_| {
            Box::pin(async {
                RUNS.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"staff": true}))
            })
        },
    );
}

const DECLARED: &str = r#"
[[functions]]
module = "guests"
name = "hello"
http = ["GET"]
guest = true

[[functions]]
module = "guests"
name = "write"
http = ["POST"]
guest = true

[[functions]]
module = "guests"
name = "limited"
http = ["GET"]
guest = true

[[functions]]
module = "guests"
name = "staff_only"
http = ["GET"]
"#;

fn declare(env: &Env) {
    let path = env.root.join("sample/app.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(DECLARED);
    std::fs::write(path, text).unwrap();
}

/// The error object without the per-request id, so two refusals compare as equal.
fn error_of(reply: &Reply) -> Json {
    let mut error = reply.body["error"].clone();
    if let Some(map) = error.as_object_mut() {
        map.remove("request_id");
        // The message names the function the caller asked for; the two names are the only
        // difference the caller may see.
        if let Some(message) = map.get("message").and_then(Json::as_str) {
            let same = message
                .replace("staff_only", "F")
                .replace("no_such_function", "F");
            map.insert("message".to_owned(), Json::String(same));
        }
        if let Some(details) = map.get_mut("details").and_then(Json::as_object_mut) {
            details.remove("request_id");
        }
    }
    error
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guest_function_is_served_to_a_request_with_no_credential() {
    register_handlers();
    let Some(env) = Env::new().await else { return };
    let secret = text_of(&env.ok(&["new-site", &env.site]));
    declare(&env);
    let host = env.site.clone();
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-06 10:00:00 UTC)));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(
        &env.root,
        false,
        Options {
            clock: Some(clock.clone() as Arc<dyn spot_core::Clock>),
            login: LoginConfig::for_tests(),
        },
        async {
            let _ = stopped.await;
        },
    )
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let anon = |method: &'static str, path: String| {
        let host = host.clone();
        async move { request(addr, &host, None, method, &path, None).await }
    };

    // A guest function by GET with no credential is served, with no user.
    let before = RUNS.load(Ordering::SeqCst);
    let hello = anon("GET", format!("{BASE}/hello")).await;
    assert_eq!(hello.status, 200, "{}", hello.raw);
    assert_eq!(hello.body["data"]["signed_in"], json!(false));
    assert_eq!(RUNS.load(Ordering::SeqCst), before + 1);

    // The same function over POST, and a guest function that writes, are refused by the guest
    // write rule: the handler does not run.
    let before = RUNS.load(Ordering::SeqCst);
    let post = anon("POST", format!("{BASE}/hello")).await;
    assert!(post.status == 401 || post.status == 405, "{}", post.raw);
    let write = anon("POST", format!("{BASE}/write")).await;
    assert_eq!(write.status, 401, "{}", write.raw);
    assert_eq!(
        write.body["error"]["details"]["function_code"],
        json!("function.auth_required")
    );
    assert_eq!(RUNS.load(Ordering::SeqCst), before);

    // A non-guest function is exactly an unknown function to an anonymous caller.
    let staff = anon("GET", format!("{BASE}/staff_only")).await;
    let unknown = anon("GET", format!("{BASE}/no_such_function")).await;
    assert_eq!(staff.status, 404, "{}", staff.raw);
    assert_eq!(unknown.status, 404, "{}", unknown.raw);
    assert_eq!(staff.body["error"]["code"], json!("method.not_found"));
    assert_eq!(error_of(&staff), error_of(&unknown));

    // The guest rate limit over HTTP: two pass, the third is 429 with Retry-After. Every
    // anonymous caller shares the one `anonymous` bucket (each request here is a new connection).
    for _ in 0..2 {
        let reply = anon("GET", format!("{BASE}/limited")).await;
        assert_eq!(reply.status, 200, "{}", reply.raw);
    }
    let limited = anon("GET", format!("{BASE}/limited")).await;
    assert_eq!(limited.status, 429, "{}", limited.raw);
    assert!(
        limited.raw.to_ascii_lowercase().contains("retry-after: "),
        "{}",
        limited.raw
    );

    // A credential that does not resolve is refused, never served as a guest, and the handler
    // never runs: malformed, revoked, and a session that still has to choose its password.
    let before = RUNS.load(Ordering::SeqCst);
    let malformed = request(
        addr,
        &host,
        Some("not-a-token"),
        "GET",
        &format!("{BASE}/hello"),
        None,
    )
    .await;
    assert!(
        malformed.status == 401 || malformed.status == 403,
        "{}",
        malformed.raw
    );
    let login = |password: &str| json!({"user": "administrator", "password": password});
    let first = request(
        addr,
        &host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(login(&secret)),
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let first_token = first.body["data"]["token"].as_str().unwrap().to_owned();
    let restricted = request(
        addr,
        &host,
        Some(&first_token),
        "GET",
        &format!("{BASE}/hello"),
        None,
    )
    .await;
    assert!(
        restricted.status == 401 || restricted.status == 403,
        "{}",
        restricted.raw
    );
    let admin = administrator(addr, &host, &secret).await;
    let out = request(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/auth/logout",
        None,
    )
    .await;
    assert!(out.status == 200 || out.status == 204, "{}", out.raw);
    let revoked = request(
        addr,
        &host,
        Some(&admin),
        "GET",
        &format!("{BASE}/hello"),
        None,
    )
    .await;
    assert!(
        revoked.status == 401 || revoked.status == 403,
        "{}",
        revoked.raw
    );
    assert_eq!(
        RUNS.load(Ordering::SeqCst),
        before,
        "a refused credential ran a handler"
    );

    let _ = stop.send(());
    server.join().await.unwrap();
}
