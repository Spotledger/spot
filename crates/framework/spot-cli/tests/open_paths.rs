//! The open-path allow-list through a real host (WP-PRD-01.T33): over real PostgreSQL and real
//! HTTP, a request without a session is refused on a normal route, a declared open path is
//! reachable without one (exact method and pattern, rate limited, audited without its
//! parameter), and an undeclared or reserved path cannot be opened.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use axum::Router;
use axum::routing::{get, post};
use http::Method;
use spot_cli::services::{
    OpenPath, RateLimit, RegisterFuture, Registration, SiteCtx, register_all,
};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the probe's open route answers.
const FIXED: &str = r#"{"probe":"hook-ran"}"#;
/// A path parameter that is a secret: it must never reach an audit record.
const SECRET_ID: &str = "hook-secret-9f3a7c";

// ---- a capturing tracing subscriber (as in password_gate.rs) -------------------------------

static CAPTURED: OnceLock<Arc<Mutex<String>>> = OnceLock::new();

struct Capture {
    out: Arc<Mutex<String>>,
    next: AtomicU64,
}

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut line = format!("span {}", attributes.metadata().name());
        attributes.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
        tracing::span::Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::from("record");
        values.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!("event {}", event.metadata().target());
        event.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn capture() -> Arc<Mutex<String>> {
    CAPTURED
        .get_or_init(|| {
            let out = Arc::new(Mutex::new(String::new()));
            let _ = tracing::subscriber::set_global_default(Capture {
                out: out.clone(),
                next: AtomicU64::new(0),
            });
            out
        })
        .clone()
}

// ---- the probe modules ---------------------------------------------------------------------

fn hook(pattern: &'static str) -> OpenPath {
    OpenPath {
        method: Method::POST,
        pattern,
        limit: RateLimit { per_minute: 3 },
    }
}

/// The probe: one declared open route (3 per minute) and one route it does not declare.
fn probe<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        ctx.open_paths(&[hook("/api/v1/probe/hook/{id}")])?;
        ctx.router_layer(|router: Router| {
            router
                .route(
                    "/api/v1/probe/hook/{id}",
                    post(|| async { ([("content-type", "application/json")], FIXED) }),
                )
                .route("/api/v1/probe/private", get(|| async { "private" }))
        });
        Ok(())
    })
}

fn reserved_metrics<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move { ctx.open_paths(&[hook("/metrics")]) })
}
fn reserved_realtime<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move { ctx.open_paths(&[hook("/api/v1/realtime")]) })
}
fn reserved_login<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move { ctx.open_paths(&[hook("/api/v1/auth/login")]) })
}
/// The same method and pattern as `probe` (the parameter's name does not make it another route).
fn twin<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move { ctx.open_paths(&[hook("/api/v1/probe/hook/{other}")]) })
}

const fn module(name: &'static str, register: spot_cli::services::RegisterFn) -> Registration {
    Registration {
        name,
        order: 5000,
        register,
    }
}

static PROBE: &[Registration] = &[module("open_probe", probe)];
static BAD_METRICS: &[Registration] = &[module("bad_metrics", reserved_metrics)];
static BAD_REALTIME: &[Registration] = &[module("bad_realtime", reserved_realtime)];
static BAD_LOGIN: &[Registration] = &[module("bad_login", reserved_login)];
static DUPLICATE: &[Registration] = &[module("open_probe", probe), module("open_twin", twin)];

// ---- helpers -------------------------------------------------------------------------------

struct Reply {
    status: u16,
    body: String,
    raw: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        let head = self.raw.split_once("\r\n\r\n")?.0;
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
}

async fn http(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
) -> Reply {
    http_body(addr, host, token, method, path, "").await
}

async fn http_body(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: &str,
) -> Reply {
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
        body: payload.to_owned(),
        raw: raw.clone(),
    }
}

/// A full session: the first sign-in secret only chooses a password, and the session that
/// replaces it is an ordinary one.
async fn full_session(addr: SocketAddr, host: &str, first: &str) -> String {
    let login = http_body(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        &serde_json::json!({"user": "administrator", "password": first}).to_string(),
    )
    .await;
    assert_eq!(login.status, 200, "{}", login.raw);
    let token = |reply: &Reply| {
        serde_json::from_str::<serde_json::Value>(&reply.body).unwrap()["data"]["token"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let restricted = token(&login);
    let changed = http_body(
        addr,
        host,
        Some(&restricted),
        "POST",
        "/api/v1/auth/password",
        &serde_json::json!({"current": first, "new": "violet-lantern-staple-9-Quartz"}).to_string(),
    )
    .await;
    assert_eq!(changed.status, 200, "{}", changed.raw);
    token(&changed)
}

fn options(clock: &Arc<spot_core::ManualClock>) -> Options {
    Options {
        clock: Some(clock.clone() as Arc<dyn spot_core::Clock>),
        login: LoginConfig::for_tests(),
    }
}

async fn start(
    env: &Env,
    clock: &Arc<spot_core::ManualClock>,
    modules: &'static [Registration],
) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running =
        spot_cli::start_with_modules(&env.root, false, options(clock), None, modules, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

fn clock() -> Arc<spot_core::ManualClock> {
    Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()))
}

fn error_code(reply: &Reply) -> String {
    serde_json::from_str::<serde_json::Value>(&reply.body)
        .ok()
        .and_then(|v| v["error"]["code"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The text a failing registration gives the operator (what the host prints when it refuses to
/// serve the site), from the same `register_all` the host's site build runs.
async fn registration_error(table: &'static [Registration]) -> String {
    let mut ctx = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        true,
    );
    register_all(&mut ctx, &[table])
        .await
        .unwrap_err()
        .to_string()
}

// ---- the tests -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_declared_open_path_is_reachable_without_a_session() {
    let log = capture();
    let Some(env) = Env::new().await else { return };
    let first = env.ok(&["new-site", &env.site]);
    let first = first.trim().rsplit(' ').next().unwrap().to_owned();
    let clock = clock();
    let (server, addr, stop) = start(&env, &clock, PROBE).await;
    let host = env.site.clone();
    let key = full_session(addr, &host, &first).await;

    // A normal native route and the probe's undeclared route need a credential.
    for path in ["/api/v1/meta/task", "/api/v1/probe/private"] {
        let reply = http(addr, &host, None, "GET", path).await;
        assert_eq!(reply.status, 401, "{path}: {}", reply.raw);
        assert_eq!(error_code(&reply), "auth.required", "{path}: {}", reply.raw);
        assert!(!reply.body.contains("private"), "{path}");
    }
    // The route exists: only authentication differs.
    let private = http(addr, &host, Some(&key), "GET", "/api/v1/probe/private").await;
    assert_eq!(private.status, 200, "{}", private.raw);
    assert_eq!(private.body, "private");

    // The declared method and pattern: reachable without any credential.
    let hook = format!("/api/v1/probe/hook/{SECRET_ID}");
    let opened = http(addr, &host, None, "POST", &hook).await;
    assert_eq!(opened.status, 200, "{}", opened.raw);
    assert_eq!(opened.body, FIXED);

    // Only that method and that shape: everything else keeps the authenticate-first rule.
    for (method, path) in [
        ("GET", hook.as_str()),
        ("POST", "/api/v1/probe/hook/abc/extra"),
        ("POST", "/api/v1/probe/hook"),
        ("POST", "/api/v1/probe/hook/"),
        ("PUT", hook.as_str()),
        ("POST", "/api/v1/probe/private"),
    ] {
        let reply = http(addr, &host, None, method, path).await;
        assert_eq!(reply.status, 401, "{method} {path}: {}", reply.raw);
        assert!(!reply.body.contains("hook-ran"), "{method} {path}");
    }

    // 3 per minute: the first call above was one; two more pass, the fourth is refused with a
    // Retry-After, and a minute later the budget is back.
    for _ in 0..2 {
        assert_eq!(http(addr, &host, None, "POST", &hook).await.status, 200);
    }
    let limited = http(addr, &host, None, "POST", &hook).await;
    assert_eq!(limited.status, 429, "{}", limited.raw);
    assert!(
        limited
            .header("retry-after")
            .and_then(|v| v.parse::<u32>().ok())
            .is_some_and(|seconds| (1..=60).contains(&seconds)),
        "{}",
        limited.raw
    );
    assert!(!limited.body.contains("hook-ran"));
    clock.advance(Duration::from_secs(60));
    let again = http(addr, &host, None, "POST", &hook).await;
    assert_eq!(again.status, 200, "{}", again.raw);
    assert_eq!(again.body, FIXED);

    // The operator can see the allow-list.
    assert_eq!(
        server.open_paths(),
        ["POST /api/v1/probe/hook/{id} (open_probe)"]
    );
    let ready = http(addr, &host, None, "GET", "/api/health/ready").await;
    assert_eq!(ready.status, 200, "{}", ready.raw);

    stop.send(()).unwrap();
    server.handle.join().await.unwrap();

    // The audit records name the declared pattern and the outcome, never the secret parameter.
    tracing::info!("canary");
    let logged = log.lock().unwrap().clone();
    assert!(logged.contains("canary"), "the capture works");
    let records: Vec<&str> = logged
        .lines()
        .filter(|line| line.starts_with("event spot::audit::open_path"))
        .filter(|line| line.contains(&format!("site={}", env.site)))
        .collect();
    assert_eq!(records.len(), 5, "{records:?}");
    assert_eq!(
        records.iter().filter(|r| r.contains("RateLimited")).count(),
        1
    );
    for record in &records {
        assert!(record.contains("/api/v1/probe/hook/{id}"), "{record}");
        assert!(record.contains("open_probe"), "{record}");
    }
    // The audit records never carry the parameter; the key and the handler's body appear in no
    // log line at all. (The `spot_http_request` span does record the concrete request path, so
    // a secret path parameter is in it: reported as a known security issue, not asserted here.)
    for record in &records {
        assert!(
            !record.contains(SECRET_ID),
            "the parameter reached an audit record"
        );
    }
    for needle in [key.as_str(), "hook-ran"] {
        let hit = logged.lines().find(|l| l.contains(needle));
        assert!(hit.is_none(), "a secret reached a log line: {hit:?}");
    }
}

/// A site whose module declares a reserved path does not serve: the host answers its requests
/// with `503 site_unavailable` (the build error goes to the operator, not the caller).
async fn refuses_to_serve(modules: &'static [Registration]) {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let clock = clock();
    let (server, addr, stop) = start(&env, &clock, modules).await;
    let reply = http(addr, &env.site, None, "GET", "/api/v1/probe/private").await;
    assert_eq!(reply.status, 503, "{}", reply.raw);
    assert_eq!(error_code(&reply), "server.unavailable", "{}", reply.raw);
    assert!(server.open_paths().is_empty());
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_declaring_metrics_as_open_stops_the_site() {
    let error = registration_error(BAD_METRICS).await;
    assert!(
        error.contains("bad_metrics") && error.contains("/metrics"),
        "{error}"
    );
    refuses_to_serve(BAD_METRICS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_declaring_the_realtime_path_as_open_stops_the_site() {
    let error = registration_error(BAD_REALTIME).await;
    assert!(
        error.contains("bad_realtime") && error.contains("/api/v1/realtime"),
        "{error}"
    );
    refuses_to_serve(BAD_REALTIME).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_declaring_the_login_path_as_open_stops_the_site() {
    let error = registration_error(BAD_LOGIN).await;
    assert!(
        error.contains("bad_login") && error.contains("/api/v1/auth/login"),
        "{error}"
    );
    refuses_to_serve(BAD_LOGIN).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_modules_declaring_the_same_open_path_stop_the_site() {
    // `open_probe` runs first (same order, name ties), so the twin is the second declarer.
    let error = registration_error(DUPLICATE).await;
    assert!(
        error.contains("open_probe") && error.contains("open_twin"),
        "{error}"
    );
    assert!(error.contains("/api/v1/probe/hook/"), "{error}");
    refuses_to_serve(DUPLICATE).await;
}
