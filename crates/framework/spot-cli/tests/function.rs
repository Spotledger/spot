//! Module-level whitelisted functions in `spot serve` (WP-PLT-17.T2), over real HTTP and the test
//! PostgreSQL: an installed app declares `[[functions]]`, the binary registers native handlers, and
//! a signed-in caller reaches them through `POST|GET /api/v1/method/{app}/{module}/{function}` with
//! the rules of `spot-functions`: the right verb and `Allow` set, an argument schema that names the
//! problem without echoing the value, a rate limit that ends in `429` with `Retry-After` and
//! returns when the (manual) clock moves, a handler's own refusal passing through as itself, and a
//! guest never reaching a function. Two declarations of one function stop the site start, naming
//! it; a reload brings in a function the app gained.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_app_api::AppError;
use spot_app_api::error::ErrorClass;
use spot_cli::Options;
use spot_cli::natives::{self, FunctionFailure, FunctionSpec};
use spot_core::ManualClock;
use spot_functions::{Context, FunctionHandler, HandlerFuture};
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

fn error_code(reply: &Reply) -> &str {
    reply.body["error"]["code"].as_str().unwrap_or_default()
}

fn function_code(reply: &Reply) -> &str {
    reply.body["error"]["details"]["function_code"]
        .as_str()
        .unwrap_or_default()
}

fn text_of(out: &str) -> String {
    out.trim().rsplit(' ').next().unwrap().to_owned()
}

const BASE: &str = "/api/v1/method/sample/reports";

/// Counts the calls that reached a handler.
static STAMPS: AtomicUsize = AtomicUsize::new(0);

/// A handler that refuses as itself: the caller lacks a right the function needs.
struct Refuse;
impl FunctionHandler for Refuse {
    fn call<'a>(&'a self, _ctx: &'a Context, _args: Json) -> HandlerFuture<'a> {
        Box::pin(async {
            Err(AppError::new(
                ErrorClass::PermissionDenied,
                "sample.no_right",
                "this function needs a right you do not hold",
            ))
        })
    }
}

fn register_handlers() {
    natives::register_function_fn(
        "sample",
        "reports",
        "echo",
        FunctionSpec {
            limit: None,
            arguments: Some(json!({
                "type": "object",
                "required": ["n"],
                "properties": {"n": {"type": "integer"}}
            })),
        },
        |call| {
            Box::pin(async move {
                Ok(json!({
                    "n": call.args["n"],
                    "tenant": call.tenant,
                    "signed_in": call.user.is_some(),
                    "verb": call.verb,
                }))
            })
        },
    );
    natives::register_function_fn(
        "sample",
        "reports",
        "stamp",
        FunctionSpec::default(),
        |_| {
            Box::pin(async {
                STAMPS.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"stamped": true}))
            })
        },
    );
    natives::register_function_fn(
        "sample",
        "reports",
        "limited",
        FunctionSpec {
            limit: Some((2, Duration::from_secs(60))),
            arguments: None,
        },
        |_| Box::pin(async { Ok(json!({"ok": true})) }),
    );
    natives::register_function_fn(
        "sample",
        "reports",
        "broken",
        FunctionSpec::default(),
        |_| {
            Box::pin(async {
                Err(FunctionFailure {
                    code: "sample.nothing_to_report".to_owned(),
                    message: "there is nothing to report".to_owned(),
                    denied: false,
                })
            })
        },
    );
    natives::register_function("sample", "reports", "refuse", Arc::new(Refuse));
}

const DECLARED: &str = r#"
[[functions]]
module = "reports"
name = "echo"
http = ["GET"]

[[functions]]
module = "reports"
name = "stamp"
http = ["POST"]

[[functions]]
module = "reports"
name = "limited"
http = ["GET"]

[[functions]]
module = "reports"
name = "broken"
http = ["POST"]

[[functions]]
module = "reports"
name = "refuse"
http = ["POST"]

[[functions]]
module = "reports"
name = "undeclared_handler"
http = ["GET"]

[[functions]]
module = "reports"
name = "job_only"
"#;

fn declare(env: &Env, extra: &str) {
    let path = env.root.join("sample/app.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(DECLARED);
    text.push_str(extra);
    std::fs::write(path, text).unwrap();
}

struct Node {
    server: spot_cli::Running,
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
}

async fn start(env: &Env, clock: Arc<ManualClock>) -> Node {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(
        &env.root,
        false,
        Options {
            clock: Some(clock as Arc<dyn spot_core::Clock>),
            login: LoginConfig::for_tests(),
        },
        async {
            let _ = stopped.await;
        },
    )
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    Node { server, addr, stop }
}

impl Node {
    async fn finish(self) {
        let _ = self.stop.send(());
        self.server.join().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn functions_are_served_with_the_rules_of_the_registry() {
    register_handlers();
    let Some(env) = Env::new().await else { return };
    let secret = text_of(&env.ok(&["new-site", &env.site]));
    declare(&env, "");
    let host = env.site.clone();
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-06 10:00:00 UTC)));
    let node = start(&env, clock.clone()).await;
    let addr = node.addr;
    let admin = administrator(addr, &host, &secret).await;
    let call = |method: &'static str, path: String, body: Option<Json>| {
        let (host, admin) = (host.clone(), admin.clone());
        async move { request(addr, &host, Some(&admin), method, &path, body).await }
    };

    // The success path: GET arguments are read by the schema (`n` is an integer), the handler
    // sees the tenant, the verb and a signed-in caller.
    let ok = call("GET", format!("{BASE}/echo?n=7"), None).await;
    assert_eq!(ok.status, 200, "{}", ok.raw);
    assert_eq!(ok.body["data"]["n"], json!(7));
    assert_eq!(ok.body["data"]["tenant"], json!(host));
    assert_eq!(ok.body["data"]["signed_in"], json!(true));
    assert_eq!(ok.body["data"]["verb"], json!("GET"));

    // The write function over POST mutates (a counted call); GET is not one of its verbs.
    let before = STAMPS.load(Ordering::SeqCst);
    let stamped = call("POST", format!("{BASE}/stamp"), Some(json!({"args": {}}))).await;
    assert_eq!(stamped.status, 200, "{}", stamped.raw);
    assert_eq!(stamped.body["data"]["stamped"], json!(true));
    assert_eq!(STAMPS.load(Ordering::SeqCst), before + 1);
    let wrong = call("GET", format!("{BASE}/stamp"), None).await;
    assert_eq!(wrong.status, 405, "{}", wrong.raw);
    assert_eq!(error_code(&wrong), "api.method_not_allowed");
    assert_eq!(function_code(&wrong), "function.verb_not_allowed");
    assert_eq!(wrong.body["error"]["details"]["allow"], json!("POST"));
    assert_eq!(
        STAMPS.load(Ordering::SeqCst),
        before + 1,
        "a refused verb ran nothing"
    );
    let wrong = call(
        "POST",
        format!("{BASE}/echo"),
        Some(json!({"args": {"n": 1}})),
    )
    .await;
    assert_eq!(wrong.status, 405, "{}", wrong.raw);
    assert_eq!(wrong.body["error"]["details"]["allow"], json!("GET"));

    // Bad arguments name the problem and never echo the value.
    let missing = call("GET", format!("{BASE}/echo"), None).await;
    assert_eq!(missing.status, 400, "{}", missing.raw);
    assert_eq!(error_code(&missing), "api.invalid_request");
    assert_eq!(function_code(&missing), "function.bad_arguments");
    assert!(missing.raw.contains("n"), "{}", missing.raw);
    let wrong_type = call("GET", format!("{BASE}/echo?n=SECRET-VALUE-77"), None).await;
    assert_eq!(wrong_type.status, 400, "{}", wrong_type.raw);
    assert_eq!(function_code(&wrong_type), "function.bad_arguments");
    assert!(
        !wrong_type.raw.contains("SECRET-VALUE-77"),
        "an argument value was echoed: {}",
        wrong_type.raw
    );

    // Unknown, undeclared-handler, job-only and foreign-app functions all answer `not_found`
    // (a declared function without a handler is on the list but answers like an unknown one).
    for path in [
        format!("{BASE}/nothing"),
        format!("{BASE}/undeclared_handler"),
        "/api/v1/method/other/reports/echo".to_owned(),
        format!("{BASE}/job_only"),
    ] {
        let reply = call("GET", path.clone(), None).await;
        assert_eq!(
            reply.status,
            if path.ends_with("job_only") { 405 } else { 404 },
            "{path}: {}",
            reply.raw
        );
    }
    let unknown = call("GET", format!("{BASE}/nothing"), None).await;
    assert_eq!(error_code(&unknown), "method.not_found");
    assert_eq!(function_code(&unknown), "function.not_found");

    // A handler's own refusals pass through as themselves: not an empty success.
    let refused = call("POST", format!("{BASE}/refuse"), Some(json!({"args": {}}))).await;
    assert_eq!(refused.status, 403, "{}", refused.raw);
    assert_eq!(error_code(&refused), "permission.denied");
    let broken = call("POST", format!("{BASE}/broken"), Some(json!({"args": {}}))).await;
    assert_eq!(broken.status, 422, "{}", broken.raw);

    // Nobody signed in never reaches a function.
    let before = STAMPS.load(Ordering::SeqCst);
    let anonymous = request(
        addr,
        &host,
        None,
        "POST",
        &format!("{BASE}/stamp"),
        Some(json!({"args": {}})),
    )
    .await;
    assert!(
        anonymous.status == 401 || anonymous.status == 404,
        "{}",
        anonymous.raw
    );
    assert_eq!(STAMPS.load(Ordering::SeqCst), before);

    // The rate limit: two calls pass, the third is `429` with `Retry-After`, a minute later the
    // bucket has refilled (the clock is manual: no sleeping).
    for _ in 0..2 {
        let reply = call("GET", format!("{BASE}/limited"), None).await;
        assert_eq!(reply.status, 200, "{}", reply.raw);
    }
    let limited = call("GET", format!("{BASE}/limited"), None).await;
    assert_eq!(limited.status, 429, "{}", limited.raw);
    assert_eq!(error_code(&limited), "api.rate_limited");
    assert_eq!(function_code(&limited), "function.rate_limited");
    assert!(
        limited.raw.to_ascii_lowercase().contains("retry-after: "),
        "{}",
        limited.raw
    );
    clock.advance(Duration::from_secs(61));
    let again = call("GET", format!("{BASE}/limited"), None).await;
    assert_eq!(again.status, 200, "{}", again.raw);
    node.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_serves_a_function_the_app_gained() {
    register_handlers();
    natives::register_function_fn(
        "sample",
        "reports",
        "gained",
        FunctionSpec::default(),
        |_| Box::pin(async { Ok(json!({"gained": true})) }),
    );
    let Some(env) = Env::new().await else { return };
    let secret = text_of(&env.ok(&["new-site", &env.site]));
    declare(&env, "");
    let host = env.site.clone();
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-06 10:00:00 UTC)));
    let node = start(&env, clock).await;
    let addr = node.addr;
    let admin = administrator(addr, &host, &secret).await;
    let path = format!("{BASE}/gained");
    let before = request(addr, &host, Some(&admin), "GET", &path, None).await;
    assert_eq!(before.status, 404, "{}", before.raw);

    let file = env.root.join("sample/app.toml");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("\n[[functions]]\nmodule = \"reports\"\nname = \"gained\"\nhttp = [\"GET\"]\n");
    std::fs::write(&file, text).unwrap();
    spot_cli::request_reload(&env.root, &env.site).unwrap();
    let mut served = None;
    for _ in 0..120 {
        let reply = request(addr, &host, Some(&admin), "GET", &path, None).await;
        if reply.status == 200 {
            served = Some(reply);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let served = served.expect("the reloaded site never served the new function");
    assert_eq!(served.body["data"]["gained"], json!(true));
    node.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_declarations_of_one_function_stop_the_site_start() {
    register_handlers();
    let Some(env) = Env::new().await else { return };
    let _secret = text_of(&env.ok(&["new-site", &env.site]));
    declare(&env, "");
    let file = env.root.join("sample/app.toml");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("\n[[functions]]\nmodule = \"reports\"\nname = \"echo\"\nhttp = [\"GET\"]\n");
    std::fs::write(&file, text).unwrap();
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-06 10:00:00 UTC)));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let started = spot_cli::start_with(
        &env.root,
        false,
        Options {
            clock: Some(clock as Arc<dyn spot_core::Clock>),
            login: LoginConfig::for_tests(),
        },
        async {
            let _ = stopped.await;
        },
    )
    .await;
    match started {
        Ok(server) => {
            // A manifest check may refuse earlier, and a site that fails to build is reported by
            // readiness rather than by the start; either way the function must not be served.
            let addr = server.local_addrs()[0];
            let reply = request(addr, &env.site, None, "GET", "/api/v1/auth/session", None).await;
            assert_eq!(reply.status, 503, "{}", reply.raw);
            let _ = stop.send(());
            let _ = server.join().await;
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("echo") || message.contains("function"),
                "{message}"
            );
        }
    }
}
