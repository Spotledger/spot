//! A `{param}` open-path pattern cannot open a path the host owns (WP-PRD-01.T41).
//!
//! `OpenPaths::find` answers `None` for every reserved request path (`/metrics`, the realtime
//! upgrade, the auth routes, the password path, health) before it looks at the declared
//! entries, because a pattern with a parameter segment can match such a path. These tests fail
//! if that guard is removed: a unit table over `OpenPaths::serve`, the ways a path can be
//! written, and a real host over PostgreSQL (skips without PostgreSQL unless
//! `SPOT_REQUIRE_PG=1`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::{get, post};
use spot_cli::services::{
    MemoryOpenAudit, OpenOutcome, OpenPath, OpenPaths, RateLimit, RegisterFuture, Registration,
    SiteCtx, register_all,
};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What a host-owned handler answers in the unit router: it must never be reached through an
/// open entry.
const HOST: &str = "host-owned-handler";
/// What the open entry's handler answers.
const OPEN: &str = "open-entry-handler";

fn clock() -> Arc<spot_core::ManualClock> {
    Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()))
}

// ---- unit table (no PostgreSQL) ------------------------------------------------------------

/// A registration that declares one open path (a fresh function per use, because a
/// registration holds a function pointer).
macro_rules! open_module {
    ($name:literal, $method:expr, $pattern:literal) => {
        Registration {
            name: $name,
            order: 1000,
            register: {
                fn declare<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
                    Box::pin(async move {
                        ctx.open_paths(&[OpenPath {
                            method: $method,
                            pattern: $pattern,
                            limit: RateLimit { per_minute: 100 },
                        }])
                    })
                }
                declare
            },
        }
    };
}

/// The router the unit tests serve through: a host-owned handler on every reserved path and
/// on the control path of the open entry, and the open entry's handler on everything else.
fn router() -> Router {
    let host = || async { HOST };
    Router::new()
        .route("/metrics", get(host))
        .route("/api/health", get(host))
        .route("/api/health/ready", get(host))
        .route("/api/v1/realtime", get(host))
        .route("/api/v1/auth", get(host))
        .route("/api/v1/auth/session", get(host))
        .route("/api/v1/auth/password", post(host))
        .fallback(|| async { OPEN })
}

async fn site(table: &[Registration]) -> (Arc<OpenPaths>, Arc<MemoryOpenAudit>) {
    let audit = Arc::new(MemoryOpenAudit::default());
    let mut ctx = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        true,
    )
    .with_open_audit(audit.clone());
    register_all(&mut ctx, &[table]).await.unwrap();
    let assembled = ctx.finish(router());
    (assembled.open, audit)
}

fn request(method: &Method, path: &str) -> Request {
    Request::builder()
        .method(method.clone())
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

async fn body_of(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// One declared pattern, the reserved path it overlaps, and a path that is not reserved but
/// the same pattern matches.
struct Case {
    module: Registration,
    method: Method,
    pattern: &'static str,
    reserved: &'static str,
    control: &'static str,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            module: open_module!("any_root", Method::GET, "/{x}"),
            method: Method::GET,
            pattern: "/{x}",
            reserved: "/metrics",
            control: "/anything",
        },
        Case {
            module: open_module!("health_ready", Method::GET, "/api/{x}/ready"),
            method: Method::GET,
            pattern: "/api/{x}/ready",
            reserved: "/api/health/ready",
            control: "/api/other/ready",
        },
        Case {
            module: open_module!("v1_realtime", Method::GET, "/api/v1/{x}"),
            method: Method::GET,
            pattern: "/api/v1/{x}",
            reserved: "/api/v1/realtime",
            control: "/api/v1/anything",
        },
        Case {
            module: open_module!("v1_auth", Method::GET, "/api/v1/{x}"),
            method: Method::GET,
            pattern: "/api/v1/{x}",
            reserved: "/api/v1/auth",
            control: "/api/v1/anything",
        },
        Case {
            module: open_module!("password", Method::POST, "/{x}/v1/auth/password"),
            method: Method::POST,
            pattern: "/{x}/v1/auth/password",
            reserved: "/api/v1/auth/password",
            control: "/other/v1/auth/password",
        },
    ]
}

#[tokio::test]
async fn a_param_pattern_cannot_open_a_reserved_path() {
    for case in cases() {
        let label = format!("{} {}", case.method, case.pattern);
        let (open, audit) = site(&[case.module]).await;
        let router = router();

        // The host owns this path: handed back, never served, never audited.
        let refused = open
            .serve(request(&case.method, case.reserved), &router)
            .await;
        assert!(
            refused.is_err(),
            "{label} opened the reserved path {}",
            case.reserved
        );
        assert!(
            audit.records().is_empty(),
            "{label}: the refused reserved path was audited: {:?}",
            audit.records()
        );

        // The pattern itself is accepted: a path it matches that is not reserved is served by
        // the open entry (so the refusal above is the guard, not a dead pattern).
        let served = open
            .serve(request(&case.method, case.control), &router)
            .await
            .map_err(|_| "not open")
            .unwrap_or_else(|why| panic!("{label}: control path {} {why}", case.control));
        assert_eq!(served.status(), StatusCode::OK, "{label}");
        assert_eq!(body_of(served).await, OPEN, "{label}");
        let records = audit.records();
        assert_eq!(records.len(), 1, "{label}: {records:?}");
        assert_eq!(records[0].pattern, case.pattern, "{label}");
        assert_eq!(records[0].outcome, OpenOutcome::Served(200), "{label}");
    }
}

#[tokio::test]
async fn a_pattern_inside_the_auth_tree_is_refused_at_declaration() {
    let table = [open_module!("auth_hook", Method::POST, "/api/v1/auth/{x}")];
    let mut ctx = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        true,
    );
    let error = register_all(&mut ctx, &[&table])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("auth_hook") && error.contains("/api/v1/auth/{x}"),
        "{error}"
    );
    assert!(error.contains("reserves"), "{error}");
}

/// What a request for `path` with `method` did against the declared `GET /{x}` and
/// `GET /api/v1/{x}` entries: `None` when it was handed back, else the body the entry's router
/// answered with.
async fn observed(open: &OpenPaths, method: &Method, path: &str) -> Option<String> {
    let router = router();
    match open.serve(request(method, path), &router).await {
        Err(_) => None,
        Ok(response) => Some(body_of(response).await),
    }
}

/// For a path written another way, the request is handed back, or it reaches the open entry's
/// own handler; it must never reach a handler the host owns through an open entry.
async fn assert_not_a_bypass(open: &OpenPaths, method: &Method, path: &str) {
    if let Some(body) = observed(open, method, path).await {
        assert_ne!(
            body, HOST,
            "{method} {path} reached a host-owned handler through an open entry"
        );
    }
}

async fn broad_site() -> (Arc<OpenPaths>, Arc<MemoryOpenAudit>) {
    site(&[
        open_module!("any_root", Method::GET, "/{x}"),
        open_module!("any_v1", Method::GET, "/api/v1/{x}"),
        open_module!("any_ready", Method::GET, "/api/{x}/ready"),
    ])
    .await
}

#[tokio::test]
async fn trailing_and_doubled_slashes_do_not_open_a_reserved_path() {
    let (open, _) = broad_site().await;
    for path in [
        "/metrics/",
        "//metrics",
        "/api/v1/realtime/",
        "/api//v1/realtime",
        "/api/v1/auth/",
        "/api/health/ready/",
    ] {
        assert_not_a_bypass(&open, &Method::GET, path).await;
    }
}

#[tokio::test]
async fn another_case_does_not_open_a_reserved_path() {
    let (open, _) = broad_site().await;
    for path in [
        "/Metrics",
        "/METRICS",
        "/api/v1/Realtime",
        "/API/v1/realtime",
        "/api/v1/AUTH",
        "/api/Health/ready",
    ] {
        assert_not_a_bypass(&open, &Method::GET, path).await;
    }
}

#[tokio::test]
async fn a_percent_encoded_letter_does_not_open_a_reserved_path() {
    let (open, _) = broad_site().await;
    for path in [
        "/%6detrics",
        "/%6Detrics",
        "/api/v1/%72ealtime",
        "/api/v1/%61uth",
        "/api/%68ealth/ready",
        "/metrics%2F",
    ] {
        assert_not_a_bypass(&open, &Method::GET, path).await;
    }
}

#[tokio::test]
async fn other_methods_do_not_open_a_reserved_path() {
    let (open, audit) = broad_site().await;
    for method in [Method::HEAD, Method::OPTIONS] {
        for path in [
            "/metrics",
            "/api/v1/realtime",
            "/api/v1/auth",
            "/api/health/ready",
        ] {
            // The entries are declared for GET: another method is handed back untouched.
            assert!(
                observed(&open, &method, path).await.is_none(),
                "{method} {path} was served by an open entry"
            );
        }
    }
    assert!(audit.records().is_empty(), "{:?}", audit.records());
}

// ---- a real host over PostgreSQL -----------------------------------------------------------

/// What the probe's open route answers.
const PROBE_OPEN: &str = "probe-open-section";

/// The probe: `GET /{section}` open with a limit, which overlaps every top-level reserved
/// path (`/metrics`).
fn probe<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        ctx.open_paths(&[OpenPath {
            method: Method::GET,
            pattern: "/{section}",
            limit: RateLimit { per_minute: 50 },
        }])?;
        ctx.router_layer(|router: Router| router.route("/{section}", get(|| async { PROBE_OPEN })));
        Ok(())
    })
}

static PROBE: &[Registration] = &[Registration {
    name: "open_param_probe",
    order: 5000,
    register: probe,
}];

struct Reply {
    status: u16,
    body: String,
    raw: String,
}

async fn http(
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
/// replaces it is an ordinary one (the Administrator's).
async fn full_session(addr: SocketAddr, host: &str, first: &str) -> String {
    let login = http(
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
    let changed = http(
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

async fn start(
    env: &Env,
    modules: &'static [Registration],
) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let options = Options {
        clock: Some(clock() as Arc<dyn spot_core::Clock>),
        login: LoginConfig::for_tests(),
    };
    let running = spot_cli::start_with_modules(&env.root, false, options, None, modules, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

fn json(reply: &Reply) -> serde_json::Value {
    serde_json::from_str(&reply.body).unwrap_or(serde_json::Value::Null)
}

/// The standard error body: a code and the request id.
fn assert_standard_401(reply: &Reply, what: &str) {
    assert_eq!(reply.status, 401, "{what}: {}", reply.raw);
    let body = json(reply);
    assert!(
        body["error"]["code"].is_string(),
        "{what}: no error code: {}",
        reply.raw
    );
    assert!(
        body["error"]["request_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{what}: no error.request_id: {}",
        reply.raw
    );
    assert!(!reply.body.contains(PROBE_OPEN), "{what}: {}", reply.raw);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_param_pattern_does_not_lift_authentication_on_host_paths() {
    let Some(env) = Env::new().await else { return };
    let first = env.ok(&["new-site", &env.site]);
    let first = first.trim().rsplit(' ').next().unwrap().to_owned();
    let (server, addr, stop) = start(&env, PROBE).await;
    let host = env.site.clone();
    let admin = full_session(addr, &host, &first).await;

    // The open pattern is live: a path it matches that the host does not own is served from
    // the probe without a credential.
    let open = http(addr, &host, None, "GET", "/anything", "").await;
    assert_eq!(open.status, 200, "{}", open.raw);
    assert_eq!(open.body, PROBE_OPEN);
    assert_eq!(server.open_paths(), ["GET /{section} (open_param_probe)"]);

    // Next to it, the host's own paths keep the authenticate-first rule.
    let metrics = http(addr, &host, None, "GET", "/metrics", "").await;
    assert_standard_401(&metrics, "GET /metrics");
    let session = http(addr, &host, None, "GET", "/api/v1/auth/session", "").await;
    assert_standard_401(&session, "GET /api/v1/auth/session");
    let password = http(addr, &host, None, "POST", "/api/v1/auth/password", "{}").await;
    assert_standard_401(&password, "POST /api/v1/auth/password");
    // The realtime route is the host's own handler: today a plain request (no upgrade, no
    // credential) is refused as a bad request before any authentication, with its own message,
    // not the probe's body.
    let realtime = http(addr, &host, None, "GET", "/api/v1/realtime", "").await;
    assert_eq!(realtime.status, 400, "{}", realtime.raw);
    assert!(realtime.body.contains("WebSocket"), "{}", realtime.raw);
    assert!(!realtime.body.contains(PROBE_OPEN), "{}", realtime.raw);
    // Health keeps answering as the health route does (it is open by design, and it is the
    // host's handler, not the probe's).
    for path in ["/api/health/live", "/api/health/ready"] {
        let health = http(addr, &host, None, "GET", path, "").await;
        assert_eq!(health.status, 200, "{path}: {}", health.raw);
        assert!(!health.body.contains(PROBE_OPEN), "{path}: {}", health.raw);
    }
    // A normal native route without a credential is still refused next to the open path.
    let native = http(addr, &host, None, "GET", "/api/v1/meta/task", "").await;
    assert_standard_401(&native, "GET /api/v1/meta/task");

    // The open pattern does not shadow the handler either: the Administrator reads /metrics.
    let served = http(addr, &host, Some(&admin), "GET", "/metrics", "").await;
    assert_eq!(served.status, 200, "{}", served.raw);
    assert!(!served.body.contains(PROBE_OPEN), "{}", served.raw);
    assert!(!served.body.is_empty(), "{}", served.raw);

    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}
