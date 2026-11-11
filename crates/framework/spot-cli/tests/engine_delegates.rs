//! The function and report delegates in `spot serve` (WP-PRD-01.T48), over real HTTP and the test
//! PostgreSQL: a hosting module provides a `FunctionDelegate` and a `ReportDelegate` as ports and
//! a signed-in request reaches them with its own `RequestCtx`; with no delegate the answers are the
//! ones the engine always gave; the customise admin methods never reach a delegate; an anonymous
//! request never reaches one; a second provider of either port stops the site.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_api_services::{FunctionDelegate, ReportDelegate};
use spot_cli::Options;
use spot_cli::services::{RegisterFuture, Registration, SiteCtx, register_all};
use spot_core::ManualClock;
use spot_http::api::meta::FieldType;
use spot_http::api::service::{
    ApiError, BoxFuture, Message, ReportColumn, ReportResult, RequestCtx,
};
use spot_login::LoginConfig;
use support::Env;
use time::macros::datetime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 0: the probe provides nothing; 1: it provides both delegates; 2: its twin also provides the
/// function delegate again.
static MODE: AtomicU8 = AtomicU8::new(0);

/// What the fakes were offered: the site, whether a bearer credential was present, and the call.
static FUNCTION_CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static REPORT_CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn calls(list: &Mutex<Vec<String>>) -> Vec<String> {
    list.lock().unwrap().clone()
}

fn bearer(ctx: &RequestCtx) -> bool {
    ctx.credential.is_some()
}

struct FakeFunction;
impl FunctionDelegate for FakeFunction {
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<Message>), ApiError>> {
        FUNCTION_CALLS.lock().unwrap().push(format!(
            "{} bearer={} {app}/{module}/{function} get={via_get}",
            ctx.site,
            bearer(ctx)
        ));
        Box::pin(async move { Ok((json!({"echo": args}), Vec::new())) })
    }
}

struct FakeReport;
impl ReportDelegate for FakeReport {
    fn run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>> {
        REPORT_CALLS.lock().unwrap().push(format!(
            "{} bearer={} {report} limit={limit:?}",
            ctx.site,
            bearer(ctx)
        ));
        Box::pin(async move {
            Ok((
                ReportResult {
                    columns: vec![ReportColumn {
                        name: "n".into(),
                        label: "N".into(),
                        field_type: FieldType::Int,
                    }],
                    rows: vec![vec![filters["n"].clone()]],
                    truncated: false,
                },
                Vec::new(),
            ))
        })
    }
}

fn probe<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        if MODE.load(Ordering::SeqCst) >= 1 {
            let function: Arc<dyn FunctionDelegate> = Arc::new(FakeFunction);
            ctx.provide(function)?;
            let report: Arc<dyn ReportDelegate> = Arc::new(FakeReport);
            ctx.provide(report)?;
        }
        Ok(())
    })
}

fn twin<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        if MODE.load(Ordering::SeqCst) >= 2 {
            let function: Arc<dyn FunctionDelegate> = Arc::new(FakeFunction);
            ctx.provide(function)?;
        }
        Ok(())
    })
}

const MODULES: [Registration; 2] = [
    Registration {
        name: "probe_delegates",
        order: 1000,
        register: probe,
    },
    Registration {
        name: "probe_twin",
        order: 1100,
        register: twin,
    },
];

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

struct Node {
    server: spot_cli::Running,
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
}

async fn start(env: &Env) -> Node {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(
        &env.root,
        false,
        Options {
            clock: None,
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

fn error_code(reply: &Reply) -> &str {
    reply.body["error"]["code"].as_str().unwrap_or_default()
}

const FUNCTION: &str = "/api/v1/method/probe/mod/work";
const REPORT: &str = "/api/v1/report/probe_report/run";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegates_provided_as_ports_serve_signed_in_requests_only() {
    spot_cli::live::add_extra_modules(&MODULES);

    // The registry's own refusal of a second provider, with the same two probe modules the host
    // hosts below (this part needs no database).
    MODE.store(2, Ordering::SeqCst);
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-05 10:00:00 UTC)));
    let mut ctx = SiteCtx::new("acme", Duration::from_secs(1), clock, BTreeMap::new(), true);
    let err = register_all(&mut ctx, &[&MODULES]).await.unwrap_err();
    MODE.store(0, Ordering::SeqCst);
    assert!(
        err.0.contains("`probe_delegates`")
            && err.0.contains("`probe_twin`")
            && err.0.contains("FunctionDelegate"),
        "{err}"
    );

    // The real function and report services are off: the probe module provides both delegates
    // here, and a second provider is refused.
    let Some(env) = Env::new().await else {
        return;
    };
    let common = env.root.join("common.toml");
    let mut text = std::fs::read_to_string(&common).unwrap();
    text.push_str("[services.function]\nenabled = false\n[services.report]\nenabled = false\n");
    std::fs::write(&common, text).unwrap();
    let secret = text_of(&env.ok(&["new-site", &env.site]));
    let host = env.site.clone();

    // No delegate: the answers are the ones the engine always gave.
    MODE.store(0, Ordering::SeqCst);
    let node = start(&env).await;
    let admin = administrator(node.addr, &host, &secret).await;
    let function = request(
        node.addr,
        &host,
        Some(&admin),
        "POST",
        FUNCTION,
        Some(json!({"args": {}})),
    )
    .await;
    assert_eq!(function.status, 404, "{}", function.raw);
    assert_eq!(
        error_code(&function),
        "method.not_found",
        "{}",
        function.raw
    );
    let report = request(node.addr, &host, Some(&admin), "POST", REPORT, None).await;
    assert_eq!(report.status, 404, "{}", report.raw);
    assert_eq!(error_code(&report), "report.not_found", "{}", report.raw);
    assert!(calls(&FUNCTION_CALLS).is_empty() && calls(&REPORT_CALLS).is_empty());
    node.finish().await;

    // Both delegates provided: a signed-in request reaches them with its own context.
    MODE.store(1, Ordering::SeqCst);
    let node = start(&env).await;
    let (addr, host) = (node.addr, env.site.clone());
    let admin = administrator(addr, &host, &secret).await;

    let function = request(
        addr,
        &host,
        Some(&admin),
        "POST",
        FUNCTION,
        Some(json!({"args": {"x": 1}})),
    )
    .await;
    assert_eq!(function.status, 200, "{}", function.raw);
    assert_eq!(function.body["data"]["echo"], json!({"x": 1}));
    let via_get = request(addr, &host, Some(&admin), "GET", FUNCTION, None).await;
    assert_eq!(via_get.status, 200, "{}", via_get.raw);
    assert_eq!(
        calls(&FUNCTION_CALLS),
        [
            format!("{host} bearer=true probe/mod/work get=false"),
            format!("{host} bearer=true probe/mod/work get=true"),
        ]
    );

    let report = request(
        addr,
        &host,
        Some(&admin),
        "POST",
        REPORT,
        Some(json!({"filters": {"n": 7}, "limit": 5})),
    )
    .await;
    assert_eq!(report.status, 200, "{}", report.raw);
    assert_eq!(report.body["data"]["rows"], json!([[7]]), "{}", report.raw);
    assert_eq!(
        calls(&REPORT_CALLS),
        [format!("{host} bearer=true probe_report limit=Some(5)")]
    );

    // Anonymous and bad-token requests never reach a delegate.
    for token in [None, Some("not-a-token")] {
        let anonymous = request(
            addr,
            &host,
            token,
            "POST",
            FUNCTION,
            Some(json!({"args": {}})),
        )
        .await;
        assert_eq!(anonymous.status, 401, "{}", anonymous.raw);
        let anonymous = request(addr, &host, token, "POST", REPORT, None).await;
        assert_eq!(anonymous.status, 401, "{}", anonymous.raw);
    }
    assert_eq!(calls(&FUNCTION_CALLS).len(), 2);
    assert_eq!(calls(&REPORT_CALLS).len(), 1);

    // The customise admin methods are served by the admin path and are never offered to the
    // delegate, which also keeps the admin POST-only rule.
    let list = request(
        addr,
        &host,
        Some(&admin),
        "GET",
        "/api/v1/method/platform/customise/list",
        None,
    )
    .await;
    assert_eq!(list.status, 200, "{}", list.raw);
    assert!(list.body["data"]["entries"].is_array(), "{}", list.raw);
    let get_change = request(
        addr,
        &host,
        Some(&admin),
        "GET",
        "/api/v1/method/platform/customise/create_doctype",
        None,
    )
    .await;
    assert_eq!(get_change.status, 400, "{}", get_change.raw);
    let anonymous_admin = request(
        addr,
        &host,
        None,
        "GET",
        "/api/v1/method/platform/customise/list",
        None,
    )
    .await;
    assert_eq!(anonymous_admin.status, 401, "{}", anonymous_admin.raw);
    assert_eq!(
        calls(&FUNCTION_CALLS).len(),
        2,
        "the admin path stays first"
    );
    node.finish().await;

    // A second provider of a delegate port stops the site: it is unavailable, never half served.
    MODE.store(2, Ordering::SeqCst);
    let node = start(&env).await;
    let refused = request(
        node.addr,
        &env.site,
        None,
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    assert_eq!(refused.status, 503, "{}", refused.raw);
    node.finish().await;
    MODE.store(0, Ordering::SeqCst);
}

fn text_of(out: &str) -> String {
    out.trim().rsplit(' ').next().unwrap().to_owned()
}
