//! The connector and the redirect loop against live loopback servers and a scripted sender.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use http::{HeaderMap, HeaderValue, StatusCode, header};
use spot_egress::{
    Authorized, Bytes, DenialSink, EgressConnector, EgressError, EgressPolicy, Guard, HttpRequest,
    HttpSender, InMemorySink, MAX_REQUEST_BODY_BYTES, Method, Purpose, Response, Sender,
    TestResolver,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;
use url::Url;

/// A tiny HTTP/1.1 server: counts accepted connections and records each request head. `reply`
/// maps the request head to a full raw response.
struct Server {
    port: u16,
    connections: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
}

async fn serve(reply: impl Fn(&str) -> String + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let (c, h) = (connections.clone(), heads.clone());
    let reply = Arc::new(reply);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            c.fetch_add(1, Ordering::SeqCst);
            let (h, reply) = (h.clone(), reply.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let response = reply(&head);
                h.lock().unwrap().push(head);
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Server {
        port,
        connections,
        heads,
    }
}

fn ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn redirect(to: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

struct Rig {
    connector: EgressConnector,
    sender: HttpSender,
    resolver: Arc<TestResolver>,
    sink: Arc<InMemorySink>,
}

/// `tenant "t"` may reach `svc.test` over http on `port`, and that name may sit on loopback.
fn rig(policy_toml: &str, resolver: Arc<TestResolver>) -> Rig {
    let policy = EgressPolicy::from_toml(policy_toml).unwrap();
    let sink = Arc::new(InMemorySink::new());
    let guard = Arc::new(Guard::new(
        policy,
        resolver.clone(),
        sink.clone(),
        Arc::new(spot_core::SystemClock::new()),
    ));
    let connector = EgressConnector::new(guard, "t", Purpose::Script);
    let sender = HttpSender::new(connector.clone(), Duration::from_secs(5));
    Rig {
        connector,
        sender,
        resolver,
        sink,
    }
}

fn loopback_policy(port: u16, extra: &str) -> String {
    format!(
        "[tenants.t]\n{extra}\n[[tenants.t.allow]]\nscheme = \"http\"\nhost = \"svc.test\"\nports = [{port}]\nallow_private = [\"127.0.0.1\"]\n"
    )
}

fn resolver_for(hosts: &[(&str, &str)]) -> Arc<TestResolver> {
    let r = Arc::new(TestResolver::new());
    for (host, ip) in hosts {
        // The scripted resolver answers one entry per lookup: repeat it for every hop.
        r.add_response(host, vec![vec![ip.parse().unwrap()]; 16]);
    }
    r
}

#[tokio::test]
async fn allowed_target_connects_to_the_pinned_address_with_original_host() {
    let server = serve(|_| ok("hello")).await;
    let rig = rig(
        &loopback_policy(server.port, ""),
        resolver_for(&[("svc.test", "127.0.0.1")]),
    );
    let url: Url = format!("http://svc.test:{}/path?q=1", server.port)
        .parse()
        .unwrap();
    let response = rig
        .connector
        .follow_redirects(&rig.sender, &url, HeaderMap::new(), 5)
        .await
        .unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"hello");
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.resolver.call_count("svc.test"),
        1,
        "resolved once, never again"
    );
    let heads = server.heads.lock().unwrap();
    assert!(
        heads[0].starts_with("GET /path?q=1 HTTP/1.1"),
        "{}",
        heads[0]
    );
    let want = format!("host: svc.test:{}", server.port);
    assert!(
        heads[0].to_ascii_lowercase().contains(&want),
        "{}",
        heads[0]
    );
}

#[tokio::test]
async fn denied_loopback_target_makes_zero_connections() {
    let server = serve(|_| ok("secret")).await;
    // The name is allowed but the policy does not permit the private address behind it.
    let policy = format!(
        "[tenants.t]\n[[tenants.t.allow]]\nscheme = \"http\"\nhost = \"svc.test\"\nports = [{}]\n",
        server.port
    );
    let rig = rig(&policy, resolver_for(&[("svc.test", "127.0.0.1")]));
    for target in [
        format!("http://svc.test:{}/", server.port),
        format!("http://127.0.0.1:{}/", server.port),
        format!("http://2130706433:{}/", server.port), // decimal spelling of 127.0.0.1
        format!("http://0x7f.1:{}/", server.port),
    ] {
        let url: Url = target.parse().unwrap();
        let err = rig
            .connector
            .follow_redirects(&rig.sender, &url, HeaderMap::new(), 5)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                EgressError::AddressDenied { .. } | EgressError::PolicyDenied
            ),
            "{target}: {err:?}"
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn redirect_to_metadata_is_denied_before_any_connection() {
    let server = serve(|_| redirect("http://169.254.169.254/latest/meta-data/")).await;
    let rig = rig(
        &loopback_policy(server.port, ""),
        resolver_for(&[("svc.test", "127.0.0.1")]),
    );
    let url: Url = format!("http://svc.test:{}/", server.port).parse().unwrap();
    let err = rig
        .connector
        .follow_redirects(&rig.sender, &url, HeaderMap::new(), 5)
        .await
        .unwrap_err();
    assert!(matches!(err, EgressError::RedirectDenied), "{err:?}");
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        1,
        "only the first hop connected"
    );
}

#[tokio::test]
async fn redirect_to_another_private_host_never_reaches_it() {
    let inner = serve(|_| ok("internal")).await;
    let outer = serve({
        let to = format!("http://internal.test:{}/", inner.port);
        move |_| redirect(&to)
    })
    .await;
    // Only svc.test is allowed (with its loopback address); internal.test is not.
    let rig = rig(
        &loopback_policy(outer.port, ""),
        resolver_for(&[("svc.test", "127.0.0.1"), ("internal.test", "127.0.0.1")]),
    );
    let url: Url = format!("http://svc.test:{}/", outer.port).parse().unwrap();
    let err = rig
        .connector
        .follow_redirects(&rig.sender, &url, HeaderMap::new(), 5)
        .await
        .unwrap_err();
    assert!(matches!(err, EgressError::RedirectDenied), "{err:?}");
    assert_eq!(inner.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn redirect_to_another_port_of_an_allowed_host_is_denied() {
    let other = serve(|_| ok("other port")).await;
    let outer = serve({
        let to = format!("http://svc.test:{}/", other.port);
        move |_| redirect(&to)
    })
    .await;
    let rig = rig(
        &loopback_policy(outer.port, ""),
        resolver_for(&[("svc.test", "127.0.0.1")]),
    );
    let url: Url = format!("http://svc.test:{}/", outer.port).parse().unwrap();
    let err = rig
        .connector
        .follow_redirects(&rig.sender, &url, HeaderMap::new(), 5)
        .await
        .unwrap_err();
    assert!(matches!(err, EgressError::RedirectDenied), "{err:?}");
    assert_eq!(other.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_oversized_body_is_refused() {
    let server = serve(|_| ok(&"x".repeat(100))).await;
    let rig = rig(
        &loopback_policy(server.port, "max_response_bytes = 10"),
        resolver_for(&[("svc.test", "127.0.0.1")]),
    );
    let url: Url = format!("http://svc.test:{}/", server.port).parse().unwrap();
    let err = rig
        .connector
        .follow_redirects(&rig.sender, &url, HeaderMap::new(), 5)
        .await
        .unwrap_err();
    assert!(matches!(err, EgressError::ResponseTooLarge), "{err:?}");
}

#[tokio::test]
async fn the_tower_service_connects_only_when_authorized() {
    let server = serve(|_| ok("x")).await;
    let rig = rig(
        &loopback_policy(server.port, ""),
        resolver_for(&[("svc.test", "127.0.0.1"), ("evil.test", "127.0.0.1")]),
    );
    let good: http::Uri = format!("http://svc.test:{}/", server.port).parse().unwrap();
    let _stream = rig.connector.clone().oneshot(good).await.unwrap();
    let evil: http::Uri = format!("http://evil.test:{}/", server.port)
        .parse()
        .unwrap();
    assert!(rig.connector.clone().oneshot(evil).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

// --- Redirect rules over a scripted sender: no network, so https and public names are fine.

type Next = Box<dyn Fn(&Url) -> Option<String> + Send + Sync>;

/// A scripted [`Sender`]: records every request (URL, headers, method and body) and maps each
/// URL to an optional `Location`. The status used for a redirect is `status` (default `302`).
struct Script {
    seen: Mutex<Vec<(String, HeaderMap)>>,
    sent: Mutex<Vec<(Method, Option<Vec<u8>>)>>,
    status: StatusCode,
    next: Next,
}

#[async_trait::async_trait]
impl Sender for Script {
    async fn send_request(
        &self,
        request: &HttpRequest,
        _authorized: &Authorized,
        _max_body: u64,
    ) -> Result<Response, EgressError> {
        self.seen
            .lock()
            .unwrap()
            .push((request.url.to_string(), request.headers.clone()));
        self.sent
            .lock()
            .unwrap()
            .push((request.method, request.body.as_ref().map(|b| b.to_vec())));
        let mut response_headers = HeaderMap::new();
        let status = match (self.next)(&request.url) {
            Some(location) => {
                response_headers
                    .insert(header::LOCATION, HeaderValue::from_str(&location).unwrap());
                self.status
            }
            None => StatusCode::OK,
        };
        Ok(Response {
            status,
            headers: response_headers,
            body: b"ok".to_vec(),
        })
    }
}

fn public_rig() -> Rig {
    let policy = "[tenants.t]\n[[tenants.t.allow]]\nscheme = \"https\"\nhost = \"*.example.com\"\n";
    rig(
        policy,
        resolver_for(&[
            ("a.example.com", "93.184.216.34"),
            ("b.example.com", "93.184.216.35"),
        ]),
    )
}

fn script(next: impl Fn(&Url) -> Option<String> + Send + Sync + 'static) -> Script {
    script_status(StatusCode::FOUND, next)
}

fn script_status(
    status: StatusCode,
    next: impl Fn(&Url) -> Option<String> + Send + Sync + 'static,
) -> Script {
    Script {
        seen: Mutex::new(Vec::new()),
        sent: Mutex::new(Vec::new()),
        status,
        next: Box::new(next),
    }
}

#[tokio::test]
async fn https_to_http_downgrade_is_denied() {
    let rig = public_rig();
    let s = script(|u| (u.scheme() == "https").then(|| "http://a.example.com/".to_owned()));
    let err = rig
        .connector
        .follow_redirects(
            &s,
            &"https://a.example.com/".parse().unwrap(),
            HeaderMap::new(),
            5,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EgressError::RedirectDenied), "{err:?}");
    assert_eq!(
        s.seen.lock().unwrap().len(),
        1,
        "the http hop was never sent"
    );
}

#[tokio::test]
async fn too_many_redirects_stops_the_loop() {
    let rig = public_rig();
    let s = script(|u| Some(format!("{}x", u)));
    let err = rig
        .connector
        .follow_redirects(
            &s,
            &"https://a.example.com/".parse().unwrap(),
            HeaderMap::new(),
            3,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EgressError::TooManyRedirects), "{err:?}");
    assert_eq!(
        s.seen.lock().unwrap().len(),
        4,
        "the first request plus three hops"
    );
}

#[tokio::test]
async fn credentials_are_dropped_on_a_cross_origin_hop_only() {
    let rig = public_rig();
    let s = script(|u| match u.path() {
        "/start" => Some("/same-origin".to_owned()),
        "/same-origin" => Some("https://b.example.com/other".to_owned()),
        _ => None,
    });
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer t"));
    headers.insert(header::COOKIE, HeaderValue::from_static("s=1"));
    headers.insert(header::ACCEPT, HeaderValue::from_static("yes"));
    let response = rig
        .connector
        .follow_redirects(
            &s,
            &"https://a.example.com/start".parse().unwrap(),
            headers,
            5,
        )
        .await
        .unwrap();
    assert_eq!(response.status, StatusCode::OK);
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(
        seen[1].1.contains_key(header::AUTHORIZATION),
        "same origin keeps credentials"
    );
    assert!(!seen[2].1.contains_key(header::AUTHORIZATION));
    assert!(!seen[2].1.contains_key(header::COOKIE));
    assert!(seen[2].1.contains_key(header::ACCEPT));
}

fn secret_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-api-key", HeaderValue::from_static("k"));
    h.insert("x-auth-token", HeaderValue::from_static("t"));
    h.insert("x-amz-security-token", HeaderValue::from_static("a"));
    h.insert("x-tenant-secret", HeaderValue::from_static("s"));
    h.insert("x-tenant-id", HeaderValue::from_static("not-secret"));
    h.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"e\""));
    h.insert("x-request-id", HeaderValue::from_static("r1"));
    h
}

fn redirect_once(from_path: &'static str, to: &'static str) -> Script {
    script(move |u| (u.path() == from_path).then(|| to.to_owned()))
}

#[tokio::test]
async fn custom_secret_headers_are_dropped_on_a_cross_origin_hop() {
    let rig = public_rig();
    let s = redirect_once("/start", "https://b.example.com/other");
    let request = HttpRequest::get(
        "https://a.example.com/start".parse().unwrap(),
        secret_headers(),
    );
    rig.connector
        .follow_request_redirects(&s, request, 5)
        .await
        .unwrap();
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for name in [
        "x-api-key",
        "x-auth-token",
        "x-amz-security-token",
        "x-tenant-secret",
    ] {
        assert!(seen[0].1.contains_key(name), "{name} sent to the first hop");
        assert!(!seen[1].1.contains_key(name), "{name} leaked cross-origin");
    }
    assert!(
        !seen[1].1.contains_key("x-tenant-id"),
        "allow-list, not deny-list"
    );
    assert!(seen[1].1.contains_key(header::ACCEPT));
    assert!(seen[1].1.contains_key(header::IF_NONE_MATCH));
    assert!(seen[1].1.contains_key("x-request-id"));
}

#[tokio::test]
async fn custom_secret_headers_are_kept_on_a_same_origin_hop() {
    let rig = public_rig();
    let s = redirect_once("/start", "/next");
    let request = HttpRequest::get(
        "https://a.example.com/start".parse().unwrap(),
        secret_headers(),
    );
    rig.connector
        .follow_request_redirects(&s, request, 5)
        .await
        .unwrap();
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].1, seen[0].1, "a same-origin hop keeps every header");
}

#[tokio::test]
async fn the_safe_set_can_be_extended_but_never_with_a_sensitive_name() {
    let rig = public_rig();
    let s = redirect_once("/start", "https://b.example.com/other");
    let mut request = HttpRequest::get(
        "https://a.example.com/start".parse().unwrap(),
        secret_headers(),
    );
    request.redirect_safe_headers = vec![
        http::HeaderName::from_static("x-tenant-id"),
        http::HeaderName::from_static("x-api-key"),
        http::HeaderName::from_static("authorization"),
        http::HeaderName::from_static("x-tenant-secret"),
    ];
    rig.connector
        .follow_request_redirects(&s, request, 5)
        .await
        .unwrap();
    let seen = s.seen.lock().unwrap();
    assert!(seen[1].1.contains_key("x-tenant-id"), "extension honoured");
    for name in ["x-api-key", "authorization", "x-tenant-secret"] {
        assert!(!seen[1].1.contains_key(name), "{name} must stay stripped");
    }
}

#[tokio::test]
async fn an_https_to_http_hop_on_the_same_host_is_cross_origin() {
    let policy = "[tenants.t]\n[[tenants.t.allow]]\nscheme = \"https\"\nhost = \"*.example.com\"\n\
                  [[tenants.t.allow]]\nscheme = \"http\"\nhost = \"*.example.com\"\n";
    let rig = rig(policy, resolver_for(&[("a.example.com", "93.184.216.34")]));
    let s = script(|u| (u.scheme() == "https").then(|| "http://a.example.com/".to_owned()));
    let request = HttpRequest::get("https://a.example.com/".parse().unwrap(), secret_headers());
    rig.connector
        .follow_request_redirects(&s, request, 5)
        .await
        .unwrap();
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].1.contains_key("x-api-key"));
    assert!(!seen[1].1.contains_key("x-api-key"), "downgrade strips");
}

#[test]
fn sensitive_names_are_recognised_case_insensitively() {
    for n in [
        "Authorization",
        "COOKIE",
        "Proxy-Authorization",
        "X-Api-Key",
        "x-my-Password",
        "X-Credential-Id",
    ] {
        let name: http::HeaderName = n.parse().unwrap();
        assert!(spot_egress::is_sensitive_header(&name), "{n}");
    }
    assert!(!spot_egress::is_sensitive_header(
        &"x-request-id".parse().unwrap()
    ));
}

// --- Methods and bodies over the scripted sender, and over real loopback servers.

#[tokio::test]
async fn every_method_reaches_the_scripted_sender_with_its_body_and_headers() {
    let rig = public_rig();
    let s = script(|_| None);
    let cases = [
        (Method::Get, None),
        (Method::Post, Some(b"payload".to_vec())),
        (Method::Put, Some(b"payload".to_vec())),
        (Method::Patch, Some(b"payload".to_vec())),
        (Method::Delete, None),
    ];
    for (method, body) in &cases {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("yes"));
        let request = HttpRequest::new(
            *method,
            "https://a.example.com/hook".parse().unwrap(),
            headers,
            body.clone().map(Bytes::from),
        );
        let response = rig
            .connector
            .follow_request_redirects(&s, request, 5)
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::OK);
    }
    let sent = s.sent.lock().unwrap();
    assert_eq!(sent.len(), cases.len());
    for (i, (method, body)) in cases.iter().enumerate() {
        assert_eq!(sent[i].0, *method);
        assert_eq!(sent[i].1, *body);
    }
    let seen = s.seen.lock().unwrap();
    assert!(seen.iter().all(|(_, h)| h.contains_key(header::ACCEPT)));
}

#[tokio::test]
async fn an_oversized_request_body_is_refused_before_any_connection() {
    let server = serve(|_| ok("x")).await;
    let rig = rig(
        &loopback_policy(server.port, ""),
        resolver_for(&[("svc.test", "127.0.0.1")]),
    );
    let request = HttpRequest::new(
        Method::Post,
        format!("http://svc.test:{}/", server.port).parse().unwrap(),
        HeaderMap::new(),
        Some(Bytes::from(vec![0u8; MAX_REQUEST_BODY_BYTES as usize + 1])),
    );
    let err = rig
        .connector
        .follow_request_redirects(&rig.sender, request, 5)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EgressError::RequestTooLarge { limit } if limit == MAX_REQUEST_BODY_BYTES),
        "{err:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_post_body_reaches_the_pinned_address_with_the_original_host() {
    let server = serve(|_| ok("done")).await;
    let rig = rig(
        &loopback_policy(server.port, ""),
        resolver_for(&[("svc.test", "127.0.0.1")]),
    );
    let request = HttpRequest::new(
        Method::Post,
        format!("http://svc.test:{}/path", server.port)
            .parse()
            .unwrap(),
        HeaderMap::new(),
        Some(Bytes::from_static(b"{\"ok\":true}")),
    );
    let response = rig
        .connector
        .follow_request_redirects(&rig.sender, request, 5)
        .await
        .unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.resolver.call_count("svc.test"),
        1,
        "resolved once, then the pinned address is the only one dialled"
    );
    let heads = server.heads.lock().unwrap();
    assert!(heads[0].starts_with("POST /path HTTP/1.1"), "{}", heads[0]);
    let lower = heads[0].to_ascii_lowercase();
    assert!(
        lower.contains(&format!("host: svc.test:{}", server.port)),
        "{}",
        heads[0]
    );
    assert!(lower.contains("content-length: 11"), "{}", heads[0]);
    assert!(heads[0].contains("{\"ok\":true}"), "{}", heads[0]);
}

#[tokio::test]
async fn caller_framing_headers_cannot_smuggle_a_second_length() {
    let server = serve(|_| ok("done")).await;
    let rig = rig(
        &loopback_policy(server.port, ""),
        resolver_for(&[("svc.test", "127.0.0.1")]),
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("9999"));
    headers.insert(
        header::TRANSFER_ENCODING,
        HeaderValue::from_static("chunked"),
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert(header::HOST, HeaderValue::from_static("evil.test"));
    let request = HttpRequest::new(
        Method::Post,
        format!("http://svc.test:{}/", server.port).parse().unwrap(),
        headers,
        Some(Bytes::from_static(b"abc")),
    );
    let response = rig
        .connector
        .follow_request_redirects(&rig.sender, request, 5)
        .await
        .unwrap();
    assert_eq!(response.status, StatusCode::OK);
    let heads = server.heads.lock().unwrap();
    let lower = heads[0].to_ascii_lowercase();
    assert!(lower.contains("content-length: 3"), "{}", heads[0]);
    assert!(!lower.contains("content-length: 9999"), "{}", heads[0]);
    assert!(!lower.contains("transfer-encoding"), "{}", heads[0]);
    assert!(
        lower.contains(&format!("host: svc.test:{}", server.port)),
        "{}",
        heads[0]
    );
    assert!(!lower.contains("evil.test"), "{}", heads[0]);
}

#[tokio::test]
async fn denied_posts_are_refused_and_recorded_like_gets() {
    let server = serve(|_| ok("secret")).await;
    // svc.test is allowed by name but its loopback address is not; evil.test is not allowed at
    // all. Neither a GET nor a POST may connect, and a body must never be sent.
    let policy = format!(
        "[tenants.t]\n[[tenants.t.allow]]\nscheme = \"http\"\nhost = \"svc.test\"\nports = [{}]\n",
        server.port
    );
    let rig = rig(
        &policy,
        resolver_for(&[("svc.test", "127.0.0.1"), ("evil.test", "127.0.0.1")]),
    );
    for (host, method) in [
        ("svc.test", Method::Get),
        ("svc.test", Method::Post),
        ("evil.test", Method::Post),
    ] {
        let request = HttpRequest::new(
            method,
            format!("http://{host}:{}/", server.port).parse().unwrap(),
            HeaderMap::new(),
            (method == Method::Post).then(|| Bytes::from_static(b"secret-body")),
        );
        let err = rig
            .connector
            .follow_request_redirects(&rig.sender, request, 5)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                EgressError::AddressDenied { .. } | EgressError::PolicyDenied
            ),
            "{host} {method:?}: {err:?}"
        );
    }
    let records = rig.sink.records();
    assert_eq!(records.len(), 3, "every denial is recorded, POST or GET");
    assert!(
        records
            .iter()
            .all(|r| r.host == "svc.test" || r.host == "evil.test")
    );
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        0,
        "no body reached the server"
    );
}

#[tokio::test]
async fn a_307_after_a_post_is_not_followed_by_default() {
    let rig = public_rig();
    let s = script_status(StatusCode::TEMPORARY_REDIRECT, |_| {
        Some("https://b.example.com/next".to_owned())
    });
    let request = HttpRequest::new(
        Method::Post,
        "https://a.example.com/hook".parse().unwrap(),
        HeaderMap::new(),
        Some(Bytes::from_static(b"data")),
    );
    let response = rig
        .connector
        .follow_request_redirects(&s, request, 5)
        .await
        .unwrap();
    assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        s.seen.lock().unwrap().len(),
        1,
        "the body was not replayed to the new target"
    );
}

#[tokio::test]
async fn a_307_after_a_post_is_replayed_only_when_the_caller_opts_in() {
    let rig = public_rig();
    let s = script_status(StatusCode::TEMPORARY_REDIRECT, |u| {
        (u.host_str() == Some("a.example.com")).then(|| "https://b.example.com/next".to_owned())
    });
    let mut request = HttpRequest::new(
        Method::Post,
        "https://a.example.com/hook".parse().unwrap(),
        HeaderMap::new(),
        Some(Bytes::from_static(b"data")),
    );
    request.follow_body_redirects = true;
    let response = rig
        .connector
        .follow_request_redirects(&s, request, 5)
        .await
        .unwrap();
    assert_eq!(response.status, StatusCode::OK);
    let sent = s.sent.lock().unwrap();
    assert_eq!(sent.len(), 2, "the body was replayed after re-validation");
    assert_eq!(sent[0], (Method::Post, Some(b"data".to_vec())));
    assert_eq!(sent[1], (Method::Post, Some(b"data".to_vec())));
}

#[tokio::test]
async fn a_302_after_a_post_becomes_a_bodyless_get_after_revalidation() {
    let rig = public_rig();
    let s = script(|u| {
        (u.host_str() == Some("a.example.com")).then(|| "https://b.example.com/next".to_owned())
    });
    let request = HttpRequest::new(
        Method::Post,
        "https://a.example.com/hook".parse().unwrap(),
        HeaderMap::new(),
        Some(Bytes::from_static(b"data")),
    );
    let response = rig
        .connector
        .follow_request_redirects(&s, request, 5)
        .await
        .unwrap();
    assert_eq!(response.status, StatusCode::OK);
    let sent = s.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], (Method::Post, Some(b"data".to_vec())));
    assert_eq!(sent[1], (Method::Get, None), "the body is not resent");
}

#[tokio::test]
async fn a_302_after_a_post_to_a_denied_host_is_refused() {
    let rig = public_rig();
    let s = script(|_| Some("https://evil.example.net/".to_owned()));
    let request = HttpRequest::new(
        Method::Post,
        "https://a.example.com/hook".parse().unwrap(),
        HeaderMap::new(),
        Some(Bytes::from_static(b"data")),
    );
    let err = rig
        .connector
        .follow_request_redirects(&s, request, 5)
        .await
        .unwrap_err();
    assert!(matches!(err, EgressError::RedirectDenied), "{err:?}");
    assert_eq!(
        s.seen.lock().unwrap().len(),
        1,
        "the body was never sent to the denied host"
    );
}
