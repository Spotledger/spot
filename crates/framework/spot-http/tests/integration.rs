//! Integration tests for the `spot-http` server core (brief WP-PLT-01.T1, `Tests (tests/)`).
//!
//! Routing flows use `tower::ServiceExt::oneshot` over `ServerBuilder::build()`. Flows that need
//! real sockets (draining, idle-connection capacity) use a raw HTTP/1.1 helper over TCP — no HTTP
//! client dependency required. Timing-sensitive tests (`http.timeout`, the 2 s readiness budget)
//! run on a paused tokio clock so no test waits real seconds.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request};
use axum::routing::{get, post};
use serde_json::Value;
use spot_core::SiteId;
use spot_http::{
    CheckResult, ReadinessCheck, ServerBuilder, ServerConfig, ServerHandle, SiteContext,
    StaticSites,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tower::ServiceExt;

/// Test-only unwrapping that names the failure. The workspace denies `unwrap_used` and, via
/// `-D warnings`, treats `expect_used` (warn) as an error, so tests must go through `must()`
/// instead. Mirrors the in-crate `Must` trait, which is `pub(crate)` + `#[cfg(test)]` and
/// therefore invisible from this separate integration-test crate.
///
/// These are shared binaries-wide tests; every panic message names what broke, even though no
/// helper returns an `Option` directly to the tests.
pub(crate) trait Must<T> {
    fn must(self) -> T;
}

impl<T, E> Must<T> for Result<T, E>
where
    E: std::fmt::Debug,
{
    fn must(self) -> T {
        self.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
    }
}

impl<T> Must<T> for Option<T> {
    fn must(self) -> T {
        self.unwrap_or_else(|| panic!("unexpected None"))
    }
}

/// A parsed HTTP/1.1 response from the raw-socket client.
#[derive(Debug)]
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(entry, _)| entry == name)
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap_or_else(|_| String::from("<invalid utf8 body>"))
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).must()
    }
}

fn parse_http_response(bytes: &[u8]) -> Option<RawResponse> {
    let separator = bytes.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&bytes[..separator]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let content_length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value)
        .and_then(|value| value.parse().ok())?;
    let body_start = separator + 4;
    let body = &bytes[body_start..];
    if body.len() < content_length {
        return None;
    }
    Some(RawResponse {
        status,
        headers,
        body: body[..content_length].to_vec(),
    })
}

async fn read_response(stream: &mut TcpStream) -> RawResponse {
    read_response_with_deadline(stream, Duration::from_secs(5)).await
}

async fn read_response_with_deadline(stream: &mut TcpStream, wait: Duration) -> RawResponse {
    let read_one = read_full_response(stream);
    match tokio::time::timeout(wait, read_one).await {
        Ok(response) => response,
        Err(_) => panic!("no response within {wait:?}"),
    }
}

async fn read_full_response(stream: &mut TcpStream) -> RawResponse {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8_192];
    loop {
        if let Some(response) = parse_http_response(&buffer) {
            return response;
        }
        let read = stream.read(&mut chunk).await;
        match read {
            Ok(0) => panic!(
                "connection closed before a full response arrived ({} bytes so far: {:?})",
                buffer.len(),
                String::from_utf8_lossy(&buffer)
            ),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(error) => panic!("socket read failed: {error}"),
        }
    }
}

async fn connect(addr: SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.must()
}

/// Sends a raw HTTP/1.1 request over `stream` (headers only; body handled by callers).
async fn write_request(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
) {
    let mut text =
        format!("{method} {path} HTTP/1.1\r\nhost: {host}\r\nuser-agent: spot-http-tests\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    stream.write_all(text.as_bytes()).await.must();
}

async fn single_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
) -> RawResponse {
    let mut stream = connect(addr).await;
    write_request(&mut stream, method, path, host, headers).await;
    read_response(&mut stream).await
}

/// Opens a keep-alive connection that stays idle after one completed request.
async fn idle_connection(addr: SocketAddr, host: &str) -> TcpStream {
    let mut stream = connect(addr).await;
    write_request(&mut stream, "GET", "/hello", host, &[]).await;
    let response = read_response(&mut stream).await;
    assert_eq!(
        response.status, 200,
        "idle-connection probe did not succeed"
    );
    stream
}

async fn wait_for_open_connections(handle: &ServerHandle, wait: Duration) -> usize {
    let started = Instant::now();
    loop {
        let open = handle.open_connections();
        if open == 0 || started.elapsed() > wait {
            return open;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A loopback-only config so tests never touch a real interface; callers override other fields.
pub(crate) fn ephemeral_config() -> ServerConfig {
    ServerConfig {
        bind: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)],
        ..ServerConfig::default()
    }
}

async fn start_server(
    build: impl FnOnce(ServerConfig) -> ServerBuilder,
) -> (ServerHandle, SocketAddr) {
    let handle = build(ephemeral_config())
        .start(std::future::pending())
        .await
        .must();
    let addr = *handle.local_addrs().first().must();
    (handle, addr)
}

struct CannedCheck(&'static str, bool, &'static str);

impl ReadinessCheck for CannedCheck {
    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        let result = if self.1 {
            CheckResult::ok()
        } else {
            CheckResult::failed(self.2)
        };
        Box::pin(std::future::ready(result))
    }

    fn name(&self) -> &str {
        self.0
    }
}

/// Never resolves; exercised together with the 2 s per-check budget.
struct HangingCheck;

impl ReadinessCheck for HangingCheck {
    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        Box::pin(std::future::pending())
    }

    fn name(&self) -> &str {
        "hanging-check"
    }
}

async fn hello(SiteContext { site, .. }: SiteContext) -> String {
    site.as_str().to_owned()
}

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_millis(800)).await;
    "done"
}

async fn boom() {
    panic!("kaboom");
}

/// A POST endpoint for the body-limit tests (GET /hello would 405 a POST; the body must be
/// consumed so a chunked body actually streams through the `Limited` wrapper).
async fn echo(_body: axum::body::Bytes) -> &'static str {
    "echo"
}

fn acme_site() -> SiteId {
    SiteId::new("acme").must()
}

fn routes() -> axum::Router {
    axum::Router::new().route("/hello", get(hello))
}

fn acme_builder(config: ServerConfig) -> ServerBuilder {
    let acme = acme_site();
    ServerBuilder::new(
        config,
        StaticSites::new()
            .add("erp.acme.com", acme.clone())
            .add("acme.spotledger.app", acme.clone())
            .add_site_name(acme),
    )
    .site_routes(routes())
}

fn get_request(host: &str, path: &str) -> Request {
    axum::http::Request::builder()
        .header("host", host)
        .uri(path)
        .body(axum::body::Body::empty())
        .must()
}

fn get_request_without_host(path: &str) -> Request {
    axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .must()
}

fn forwarded_request(
    host: &str,
    path: &str,
    proxy_addr: SocketAddr,
    forwarded_host: &str,
) -> Request {
    let mut request = axum::http::Request::builder()
        .header("host", host)
        .header("x-forwarded-host", forwarded_host)
        .uri(path)
        .body(axum::body::Body::empty())
        .must();
    request.extensions_mut().insert(ConnectInfo(proxy_addr));
    request
}

async fn call(router: &axum::Router, request: Request) -> axum::response::Response {
    router.clone().oneshot(request).await.must()
}

async fn response_json(response: axum::response::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1_024)
        .await
        .must();
    (status, serde_json::from_slice(&bytes).must())
}

/// For plain-text endpoints (`/hello` answers with the site name, not JSON).
async fn response_text(response: axum::response::Response) -> (u16, String) {
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1_024)
        .await
        .must();
    (
        status,
        String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| "<invalid utf8 body>".into()),
    )
}

fn envelope_code(body: &Value) -> Option<&str> {
    body.pointer("/error/code").and_then(|value| value.as_str())
}

fn envelope_request_id(body: &Value) -> Option<&str> {
    body.pointer("/error/request_id")
        .and_then(|value| value.as_str())
}

#[tokio::test]
async fn host_resolution_table() {
    let router = acme_builder(ServerConfig::default()).build();
    for (host, should_resolve) in [
        ("acme.spotledger.app:443", true),
        ("acme.spotledger.app.", true),
        ("ACME.Spotledger.APP", true),
        ("acme.spotledger.APP:80", true),
        ("erp.acme.com", true),
        ("guest.example:3000", false),
        ("guest.spotledger.app", false),
        ("sub.acme.spotledger.app", false),
    ] {
        let response = call(&router, get_request(host, "/hello")).await;
        assert!(
            response.headers().contains_key("x-request-id"),
            "x-request-id must be on every response"
        );
        if should_resolve {
            let (status, text) = response_text(response).await;
            assert_eq!(status, 200, "host {host:?} should resolve");
            assert_eq!(text, "acme", "host {host:?} should map to the acme site");
        } else {
            let (status, body) = response_json(response).await;
            assert_eq!(status, 404, "host {host:?} should be rejected");
            assert_eq!(envelope_code(&body), Some("http.unknown_site"), "{body:?}");
            assert!(
                envelope_request_id(&body).is_some_and(|id| !id.is_empty()),
                "the envelope carries the server-generated id"
            );
        }
    }
}

#[tokio::test]
async fn fallback_site_serves_unknown_hosts_and_missing_host() {
    let config = ServerConfig {
        fallback_site: Some(SiteId::new("guest").must()),
        ..ServerConfig::default()
    };
    let acme = acme_site();
    let router = ServerBuilder::new(
        config,
        StaticSites::new()
            .add("erp.acme.com", acme.clone())
            .add_site_name(acme)
            .add_site_name(SiteId::new("guest").must()),
    )
    .site_routes(routes())
    .build();
    let (status, body) =
        response_text(call(&router, get_request("not-registered.example", "/hello")).await).await;
    assert_eq!(status, 200);
    assert_eq!(body, "guest");
    let (status, body) =
        response_text(call(&router, get_request_without_host("/hello")).await).await;
    assert_eq!(
        status, 200,
        "a missing Host header also resolves to the fallback site"
    );
    assert_eq!(body, "guest");
}

fn with_header(request: Request, name: &str, value: &str) -> Request {
    let mut request = request;
    request.headers_mut().insert(
        axum::http::HeaderName::from_lowercase(name.as_bytes()).must(),
        axum::http::HeaderValue::from_str(value).must(),
    );
    request
}

#[tokio::test]
async fn forwarded_host_only_from_trusted_proxy() {
    let proxy_ip: IpAddr = "10.0.0.7".parse().must();
    let config = ServerConfig {
        trusted_proxies: vec![proxy_ip],
        ..ServerConfig::default()
    };
    let acme = acme_site();
    let router = ServerBuilder::new(
        config,
        StaticSites::new()
            .add("erp.acme.com", acme.clone())
            .add_site_name(acme),
    )
    .site_routes(routes())
    .build();
    // A connection from a trusted proxy: the forwarded host resolves the site.
    let proxy_peer = SocketAddr::new(proxy_ip, 44_444);
    let (status, body) = response_text(
        call(
            &router,
            forwarded_request("anything.example", "/hello", proxy_peer, "erp.acme.com"),
        )
        .await,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "acme", "the forwarded host must select the site");
    // The rightmost non-empty entry (the trusted proxy's own) wins, not a client-supplied one.
    let response = call(
        &router,
        forwarded_request(
            "anything.example",
            "/hello",
            proxy_peer,
            "guest.site.example, erp.acme.com",
        ),
    )
    .await;
    let (status, body) = response_text(response).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "acme");
    // A connection from an untrusted peer: the forwarded header is ignored, and the
    // Host header (anything.example) does not resolve.
    let outsider = SocketAddr::new("203.0.113.9".parse().must(), 44_444);
    let response = call(
        &router,
        forwarded_request("anything.example", "/hello", outsider, "erp.acme.com"),
    )
    .await;
    let (status, body) = response_json(response).await;
    assert_eq!(status, 404);
    assert_eq!(envelope_code(&body), Some("http.unknown_site"));
}

#[tokio::test]
async fn request_ids_echo_replace_and_stamp_errors() {
    let router = acme_builder(ServerConfig::default()).build();
    // A valid id is echoed.
    let response = call(
        &router,
        with_header(
            get_request("acme.spotledger.app:1234", "/hello"),
            "x-request-id",
            "from-client-42",
        ),
    )
    .await;
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("from-client-42"),
        "a valid client id must be echoed"
    );
    // An invalid id is replaced with a UUID-style id.
    let response = call(
        &router,
        with_header(
            get_request("acme.spotledger.app", "/hello"),
            "x-request-id",
            "bad value with spaces",
        ),
    )
    .await;
    let echoed = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok());
    let echoed = echoed.must();
    assert!(
        echoed != "bad value with spaces",
        "an invalid id must not be echoed"
    );
    assert!(
        uuid::Uuid::parse_str(echoed).is_ok(),
        "the replacement id must be a UUID: {echoed}"
    );
    // The boundary: 128 visible characters are echoed, 129 are replaced.
    let max_id = "y".repeat(128);
    let response = call(
        &router,
        with_header(
            get_request("acme.spotledger.app", "/hello"),
            "x-request-id",
            &max_id,
        ),
    )
    .await;
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some(max_id.as_str()),
        "exactly 128 characters must be echoed"
    );
    let too_long = "y".repeat(129);
    let response = call(
        &router,
        with_header(
            get_request("acme.spotledger.app", "/hello"),
            "x-request-id",
            &too_long,
        ),
    )
    .await;
    let echoed = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok());
    let echoed = echoed.must();
    assert!(echoed != too_long, "a 129-character id must be replaced");
    assert!(
        uuid::Uuid::parse_str(echoed).is_ok(),
        "the replacement id must be a UUID: {echoed}"
    );
    // An envelope error carries the echoed id in its body.
    let response = call(
        &router,
        with_header(
            get_request("guest.site.example", "/hello"),
            "x-request-id",
            "from-client-42",
        ),
    )
    .await;
    let (status, body) = response_json(response).await;
    assert_eq!(status, 404);
    assert_eq!(envelope_request_id(&body), Some("from-client-42"));
}

fn limit_router(body_limit: u64) -> axum::Router {
    let config = ServerConfig {
        max_body_bytes: body_limit,
        ..ServerConfig::default()
    };
    acme_builder(config)
        .site_routes(axum::Router::new().route("/echo", post(echo)))
        .build()
}

#[tokio::test]
async fn body_over_limit_is_413_envelope() {
    let router = limit_router(3 * 1_024);
    let request = axum::http::Request::builder()
        .header("host", "acme.spotledger.app")
        .header("content-length", 4 * 1_024)
        .method("POST")
        .uri("/echo")
        .body(axum::body::Body::from("x".repeat(4 * 1_024)))
        .must();
    let response = call(&router, request).await;
    assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert!(response.headers().contains_key("x-request-id"));
    let (status, body) = response_json(response).await;
    assert_eq!(status, 413);
    assert_eq!(envelope_code(&body), Some("http.body_too_large"));
    assert_eq!(
        body.pointer("/error/retryable").and_then(|v| v.as_bool()),
        Some(false)
    );
    // Chunked body without Content-Length: the stream limit must produce the same envelope,
    // not the plain-text error of a body layer the handler hits first. No declared length
    // header here, exactly the shape hyper presents for a `Transfer-Encoding: chunked` POST.
    let request = axum::http::Request::builder()
        .header("host", "acme.spotledger.app")
        .header("transfer-encoding", "chunked")
        .method("POST")
        .uri("/echo")
        .body(axum::body::Body::from("x".repeat(4 * 1_024)))
        .must();
    let response = call(&router, request).await;
    assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    let (status, body) = response_json(response).await;
    assert_eq!(status, 413, "chunked over-limit bodies need the envelope");
    assert_eq!(envelope_code(&body), Some("http.body_too_large"));
    assert_eq!(
        body.pointer("/error/message").and_then(|v| v.as_str()),
        Some("The body is larger than the server allows."),
        "the envelope replaces any inner plain-text length error"
    );
    // Under the limit the endpoint answers normally.
    let request = axum::http::Request::builder()
        .header("host", "acme.spotledger.app")
        .header("content-length", 2 * 1_024)
        .method("POST")
        .uri("/echo")
        .body(axum::body::Body::from("x".repeat(2 * 1_024)))
        .must();
    let (status, body) = response_text(call(&router, request).await).await;
    assert_eq!(status, 200);
    assert_eq!(body, "echo");
}

/// Regression (review): axum's built-in 2 MiB `DefaultBodyLimit` used to
/// refuse a body *between* 2 MiB and the configured `max_body_bytes` before our stream limit
/// saw it. With the default disabled in `assemble`, `max_body_bytes` governs every body.
#[tokio::test]
async fn bodies_between_2_mib_and_the_configured_limit_are_accepted() {
    let router = limit_router(ServerConfig::default().max_body_bytes); // 10 MiB
    let request = axum::http::Request::builder()
        .header("host", "acme.spotledger.app")
        .header("content-length", 3 * 1_024 * 1_024)
        .method("POST")
        .uri("/echo")
        .body(axum::body::Body::from("x".repeat(3 * 1_024 * 1_024)))
        .must();
    let (status, body) = response_text(call(&router, request).await).await;
    assert_eq!(status, 200, "3 MiB is inside the 10 MiB configured limit");
    assert_eq!(body, "echo");
}

fn limits_router() -> axum::Router {
    let config = ServerConfig {
        request_timeout: Duration::from_millis(500),
        ..ServerConfig::default()
    };
    acme_builder(config)
        .site_routes(
            axum::Router::new()
                .route("/hello", get(hello))
                .route("/slow", get(slow))
                .route("/boom", get(boom)),
        )
        .build()
}

#[tokio::test(start_paused = true)]
async fn slow_handler_hits_the_timeout_envelope() {
    let router = limits_router();
    let response = call(&router, get_request("acme.spotledger.app", "/slow")).await;
    assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
    let (status, body) = response_json(response).await;
    assert_eq!(
        status, 504,
        "the contract registry pins http.timeout to 504"
    );
    assert_eq!(envelope_code(&body), Some("http.timeout"));
    assert_eq!(
        body.pointer("/error/retryable").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[tokio::test]
async fn panicking_handler_returns_envelope_and_recovers() {
    let response = call(
        &limits_router(),
        get_request("acme.spotledger.app", "/boom"),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(response.headers().contains_key("x-request-id"));
    let (status, body) = response_json(response).await;
    assert_eq!(status, 500);
    assert_eq!(envelope_code(&body), Some("http.internal"));
    assert_eq!(
        body.pointer("/error/message").and_then(|v| v.as_str()),
        Some("Something went wrong on the server. Your request was not processed."),
        "panic details must not leak into the body"
    );
    // The stack survives the caught panic; the next request answers normally.
    let response = call(
        &limits_router(),
        get_request("acme.spotledger.app", "/hello"),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_endpoints_over_real_sockets() {
    let passing = start_server(|config| {
        acme_builder(config).readiness_check(CannedCheck("ok-check", true, "up"))
    })
    .await;
    // The health endpoints answer regardless of host resolution — any host works.
    let live = single_request(
        passing.1,
        "GET",
        "/api/health/live",
        "any.host.example",
        &[],
    )
    .await;
    assert_eq!(live.status, 200, "{live:?}");
    assert!(live.header("x-request-id").is_some());
    assert_eq!(
        live.json().pointer("/status").and_then(|v| v.as_str()),
        Some("live")
    );
    let ready = single_request(
        passing.1,
        "GET",
        "/api/health/ready",
        "anything-else.example",
        &[],
    )
    .await;
    assert_eq!(ready.status, 200, "{ready:?}");
    assert_eq!(
        ready.json().pointer("/status").and_then(|v| v.as_str()),
        Some("ready")
    );
    assert_eq!(
        ready
            .json()
            .pointer("/checks/0/name")
            .and_then(|v| v.as_str()),
        Some("ok-check")
    );
    assert_eq!(
        ready
            .json()
            .pointer("/checks/0/ok")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    // A failing check turns /ready into 503 while /live stays 200.
    let failing = start_server(|config| {
        acme_builder(config)
            .readiness_check(CannedCheck("ok-check", true, "up"))
            .readiness_check(CannedCheck("db-check", false, "db unreachable"))
    })
    .await;
    let ready = single_request(
        failing.1,
        "GET",
        "/api/health/ready",
        "any.host.example",
        &[],
    )
    .await;
    assert_eq!(ready.status, 503, "{ready:?}");
    let body = ready.json();
    assert_eq!(
        body.pointer("/status").and_then(|v| v.as_str()),
        Some("not_ready")
    );
    assert_eq!(
        body.pointer("/checks/1/ok").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        body.pointer("/checks/1/detail").and_then(|v| v.as_str()),
        Some("db unreachable")
    );
    let live = single_request(
        failing.1,
        "GET",
        "/api/health/live",
        "any.host.example",
        &[],
    )
    .await;
    assert_eq!(live.status, 200, "/live stays 200 while a check fails");
    let _ = (passing.0, failing.0);
}
#[tokio::test]
async fn hanging_check_times_out_after_2s() {
    // Paused clock: the 2 s per-check budget fires in virtual time, no real waiting.
    tokio::time::pause();
    let router = acme_builder(ServerConfig::default())
        .readiness_check(HangingCheck)
        .build();
    let response = call(
        &router,
        get_request("any.host.example", "/api/health/ready"),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let (status, body) = response_json(response).await;
    assert_eq!(status, 503);
    assert_eq!(
        body.pointer("/status").and_then(|v| v.as_str()),
        Some("not_ready")
    );
    assert_eq!(
        body.pointer("/checks/0/name").and_then(|v| v.as_str()),
        Some("hanging-check")
    );
    assert_eq!(
        body.pointer("/checks/0/ok").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        body.pointer("/checks/0/detail").and_then(|v| v.as_str()),
        Some("timed out after 2s"),
        "the 2 s per-check budget must be visible in the detail"
    );
}

#[tokio::test]
async fn ready_returns_503_during_shutdown() {
    // The brief's Health requirement: while shutdown is in progress `/api/health/ready`
    // answers 503 so load balancers drain the node, while `/live` keeps answering 200.
    let (router, shutting_down) = acme_builder(ServerConfig::default())
        .readiness_check(CannedCheck("ok-check", true, "up"))
        .build_with_shutdown_flag();
    let before = call(
        &router,
        get_request("any.host.example", "/api/health/ready"),
    )
    .await;
    assert_eq!(before.status(), axum::http::StatusCode::OK);
    // Trigger the shutdown drain, exactly like `serve` does when its signal completes.
    shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    let ready = call(
        &router,
        get_request("any.host.example", "/api/health/ready"),
    )
    .await;
    assert_eq!(
        ready.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "ready must answer 503 once shutdown has begun"
    );
    let (status, body) = response_json(ready).await;
    assert_eq!(status, 503);
    assert_eq!(
        body.pointer("/status").and_then(|v| v.as_str()),
        Some("not_ready")
    );
    let live = call(&router, get_request("any.host.example", "/api/health/live")).await;
    assert_eq!(live.status(), axum::http::StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_shutdown_drains_in_flight_and_closes_idle() {
    let config = ServerConfig {
        request_timeout: Duration::from_secs(10),
        // Long grace on purpose: the loop must exit as soon as the connections are reaped,
        // not sit out the full grace window.
        shutdown_grace: Duration::from_secs(30),
        ..ephemeral_config()
    };
    let (trigger_tx, trigger_rx) = oneshot::channel::<()>();
    let builder = acme_builder(config).site_routes(
        axum::Router::new()
            .route("/hello", get(hello))
            .route("/slow", get(slow)),
    );
    let handle = builder
        .start(async move {
            let _ = trigger_rx.await;
        })
        .await
        .must();
    let addr = *handle.local_addrs().first().must();
    // One idle keep-alive connection and one request in flight.
    let mut idle = idle_connection(addr, "acme.spotledger.app").await;
    let mut slow_stream = connect(addr).await;
    write_request(&mut slow_stream, "GET", "/slow", "acme.spotledger.app", &[]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let triggered = Instant::now();
    trigger_tx.send(()).must();
    // The in-flight request still completes within the grace.
    let response = read_response(&mut slow_stream).await;
    assert_eq!(
        response.status, 200,
        "the in-flight request must complete during draining"
    );
    assert_eq!(response.text(), "done");
    // The idle connection is closed by the server (EOF), not answered.
    let idle_read = tokio::time::timeout(Duration::from_secs(5), idle.read(&mut [0u8; 1])).await;
    match idle_read {
        Ok(Ok(0)) => {}
        Ok(Ok(_)) => panic!("idle connection was served a body during shutdown"),
        Ok(Err(_)) => {}
        Err(_) => panic!("idle connection was not closed within 5 s of shutdown"),
    }
    // All state is reaped and the loop exits cleanly.
    assert_eq!(
        wait_for_open_connections(&handle, Duration::from_secs(10)).await,
        0
    );
    // The loop breaks as soon as draining is over and no connection is left: `join` returns
    // well before the 30 s grace elapses (it would take 30 s if the grace were slept out).
    tokio::time::timeout(Duration::from_secs(3), handle.join())
        .await
        .unwrap_or_else(|_| panic!("join() still hanging 3 s after shutdown was triggered"))
        .must();
    assert!(
        triggered.elapsed() < Duration::from_secs(5),
        "shutdown finished in {:?}; the grace window must not be slept out",
        triggered.elapsed()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_idle_connections_stay_served() {
    let (handle, addr) = start_server(acme_builder).await;
    let mut idle = Vec::with_capacity(500);
    for _ in 0..500 {
        idle.push(idle_connection(addr, "acme.spotledger.app").await);
    }
    // All accepted: the server counts (at least) the 500 idle connections.
    let open = handle.open_connections();
    assert!(
        open >= 500,
        "expected at least 500 open connections, saw {open}"
    );
    // A fresh connection is still served while 500 idle ones are held.
    let response = single_request(addr, "GET", "/hello", "acme.spotledger.app:8000", &[]).await;
    assert_eq!(
        response.status, 200,
        "the server must keep accepting fresh connections"
    );
    // Clients all hang up: the server reaps every connection.
    drop(idle);
    assert_eq!(
        wait_for_open_connections(&handle, Duration::from_secs(15)).await,
        0
    );
    // The server is still healthy afterwards.
    let after = single_request(addr, "GET", "/hello", "acme.spotledger.app", &[]).await;
    assert_eq!(after.status, 200);
}
/// The soft limit on open file descriptors, for the soak-test record.
fn max_open_files() -> String {
    std::fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() >= 4
                    && fields[0] == "Max"
                    && fields[1] == "open"
                    && fields[2] == "files"
                {
                    Some(fields[3].to_owned())
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| "unknown".into())
}

/// 10k idle connections: the number from the WP's acceptance criterion. Skipped by default
/// (a machine soak, not a CI gate); run with:
/// `cargo test -p spot-http --test integration ten_thousand -- --ignored --nocapture`
/// and record the machine and `ulimit -n` in the PR report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "soak: 10k idle connections; run with --ignored on a machine with enough file descriptors"]
async fn ten_thousand_idle_connections_stay_served() {
    println!(
        "10k soak host: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("10k soak max open files: {}", max_open_files());
    let (handle, addr) = start_server(acme_builder).await;
    let started = Instant::now();
    let mut idle: Vec<TcpStream> = Vec::with_capacity(10_000);
    for _ in 0..20 {
        let mut batch = Vec::with_capacity(500);
        for _ in 0..500 {
            batch.push(tokio::spawn(idle_connection(addr, "acme.spotledger.app")));
        }
        for task in batch {
            idle.push(task.await.must());
        }
    }
    println!(
        "10k soak: opened 10000 idle connections in {:?}",
        started.elapsed()
    );
    let open = handle.open_connections();
    assert!(
        open >= 10_000,
        "expected at least 10000 open connections, saw {open}"
    );
    let response = single_request(addr, "GET", "/hello", "acme.spotledger.app:8000", &[]).await;
    assert_eq!(
        response.status, 200,
        "the server must keep accepting fresh connections"
    );
    drop(idle);
    assert_eq!(
        wait_for_open_connections(&handle, Duration::from_secs(60)).await,
        0
    );
}
