//! Per-audience listeners (ADR-083 Barrier 2, WP-IAM-11.T3): the configuration, the startup
//! refusals, and that a consumer listener serves none of the staff routes. Real sockets on
//! loopback; generated values only.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[allow(dead_code)]
mod support;

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use spot_core::{Audience, ManualClock, SiteId};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::{ApiServices, has_hand_wired_handler, route_table, router, router_for};
use spot_http::{
    CheckResult, ListenerAudience, ListenerConfig, ReadinessCheck, ServerBuilder, ServerConfig,
    ServerError, StaticSites,
};
use support::fake::FakeApp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const HOST: &str = "erp.acme.com";

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn listener(port: u16, audience: ListenerAudience) -> ListenerConfig {
    ListenerConfig {
        bind: loopback(port),
        audience,
    }
}

fn sites() -> StaticSites {
    let site = SiteId::new("acme").unwrap();
    StaticSites::new()
        .add(HOST, site.clone())
        .add_site_name(site)
}

fn services() -> ApiServices {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store)
}

/// Counts the requests that reach a matched route of the router it is layered on.
fn counted(router: Router, counter: Arc<AtomicUsize>) -> Router {
    // The consumer surface starts empty (no table entry lists the consumer audience), and a
    // `route_layer` on a router without routes is a no-op that axum refuses: give it one.
    let router = router.route("/__counted_probe", get(|| async { "probe" }));
    router.route_layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let response: Response = next.run(request).await;
                response
            }
        },
    ))
}

/// A free loopback port (bound and released; nothing else in the test binds it meanwhile).
fn free_port() -> u16 {
    let socket = std::net::TcpListener::bind(loopback(0)).unwrap();
    socket.local_addr().unwrap().port()
}

fn sample(path: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in path.chars() {
        match c {
            '{' => {
                inside = true;
                out.push_str("sample");
            }
            '}' => inside = false,
            _ if inside => {}
            _ => out.push(c),
        }
    }
    out
}

/// One request over a fresh connection; returns the status and the body text.
async fn request(addr: SocketAddr, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let text = format!(
        "{method} {path} HTTP/1.1\r\nhost: {HOST}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(text.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

struct Named(&'static str);

impl ReadinessCheck for Named {
    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        Box::pin(async { CheckResult::ok() })
    }
    fn name(&self) -> &str {
        self.0
    }
}

async fn start(builder: ServerBuilder) -> Result<spot_http::ServerHandle, ServerError> {
    builder.start(std::future::pending()).await
}

// --- configuration --------------------------------------------------------------------------

#[test]
fn config_parses_listeners_of_both_audiences() {
    let body = r#"{
        "bind": ["127.0.0.1:9001"],
        "listeners": [
            {"bind": "127.0.0.1:9002", "audience": "staff"},
            {"bind": "127.0.0.1:9003", "audience": "consumer"}
        ]
    }"#;
    let config: ServerConfig = serde_json::from_str(body).unwrap();
    assert_eq!(config.listeners.len(), 2);
    assert_eq!(config.listeners[1].audience, ListenerAudience::Consumer);
    assert!(config.validate().is_ok());
}

#[test]
fn config_refuses_a_bad_or_missing_audience_and_unknown_fields() {
    for bad in [
        r#"{"listeners":[{"bind":"127.0.0.1:9","audience":"Staff"}]}"#,
        r#"{"listeners":[{"bind":"127.0.0.1:9","audience":"admin"}]}"#,
        r#"{"listeners":[{"bind":"127.0.0.1:9"}]}"#,
        r#"{"listeners":[{"bind":"127.0.0.1:9","audience":"staff","tls":true}]}"#,
    ] {
        assert!(serde_json::from_str::<ServerConfig>(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_pre_existing_config_parses_unchanged() {
    let config: ServerConfig = serde_json::from_str(r#"{"bind":["127.0.0.1:9000"]}"#).unwrap();
    assert!(config.listeners.is_empty());
    assert_eq!(config.all_listeners().len(), 1);
    assert_eq!(config.all_listeners()[0].audience, ListenerAudience::Staff);
}

// --- startup refusal ------------------------------------------------------------------------

async fn assert_refused_without_binding(config: ServerConfig, port: u16, with_consumer: bool) {
    let mut builder = ServerBuilder::new(config, sites()).site_routes(Router::new());
    if with_consumer {
        builder = builder.consumer_routes(Router::new());
    }
    let outcome = start(builder).await;
    assert!(
        matches!(
            outcome,
            Err(ServerError::AddressClash { .. } | ServerError::NoConsumerRouter)
        ),
        "{outcome:?}"
    );
    // Nothing was left open: the port can be bound again.
    std::net::TcpListener::bind(loopback(port)).unwrap();
}

#[tokio::test]
async fn the_same_address_twice_in_bind_is_refused() {
    let port = free_port();
    let config = ServerConfig {
        bind: vec![loopback(port), loopback(port)],
        ..ServerConfig::default()
    };
    assert_refused_without_binding(config, port, false).await;
}

#[tokio::test]
async fn the_same_address_for_staff_and_consumer_is_refused() {
    let port = free_port();
    let config = ServerConfig {
        bind: vec![loopback(port)],
        listeners: vec![listener(port, ListenerAudience::Consumer)],
        ..ServerConfig::default()
    };
    assert_refused_without_binding(config, port, true).await;
}

#[tokio::test]
async fn a_wildcard_staff_listener_overlapping_a_consumer_listener_is_refused() {
    let port = free_port();
    let config = ServerConfig {
        bind: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)],
        listeners: vec![listener(port, ListenerAudience::Consumer)],
        ..ServerConfig::default()
    };
    assert_refused_without_binding(config, port, true).await;
}

#[tokio::test]
async fn an_ipv6_wildcard_against_an_ipv4_wildcard_is_refused() {
    let port = free_port();
    let config = ServerConfig {
        bind: vec![SocketAddr::new(
            IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
            port,
        )],
        listeners: vec![ListenerConfig {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            audience: ListenerAudience::Consumer,
        }],
        ..ServerConfig::default()
    };
    assert_refused_without_binding(config, port, true).await;
}

#[tokio::test]
async fn a_consumer_listener_without_a_consumer_router_is_refused() {
    let staff_port = free_port();
    let consumer_port = free_port();
    let config = ServerConfig {
        bind: vec![loopback(staff_port)],
        listeners: vec![listener(consumer_port, ListenerAudience::Consumer)],
        ..ServerConfig::default()
    };
    let outcome = start(ServerBuilder::new(config, sites()).site_routes(Router::new())).await;
    assert!(
        matches!(outcome, Err(ServerError::NoConsumerRouter)),
        "{outcome:?}"
    );
    std::net::TcpListener::bind(loopback(staff_port)).unwrap();
    std::net::TcpListener::bind(loopback(consumer_port)).unwrap();
}

// --- what each listener serves --------------------------------------------------------------

struct Running {
    handle: spot_http::ServerHandle,
    staff: SocketAddr,
    consumer: SocketAddr,
    staff_hits: Arc<AtomicUsize>,
    consumer_hits: Arc<AtomicUsize>,
}

/// A staff listener over the real router and a consumer listener over `consumer` (the real
/// consumer router plus whatever the test adds).
async fn running(consumer: Router) -> Running {
    let staff_hits = Arc::new(AtomicUsize::new(0));
    let consumer_hits = Arc::new(AtomicUsize::new(0));
    let staff_routes = counted(router(services()), staff_hits.clone())
        .merge(Router::new().route("/scim/v2/probe", get(|| async { "scim" })));
    let config = ServerConfig {
        bind: vec![loopback(0)],
        listeners: vec![listener(0, ListenerAudience::Consumer)],
        ..ServerConfig::default()
    };
    let handle = start(
        ServerBuilder::new(config, sites())
            .site_routes(staff_routes)
            .consumer_routes(counted(consumer, consumer_hits.clone()))
            .readiness_check(Named("database")),
    )
    .await
    .unwrap();
    let staff = handle.local_addrs_for(ListenerAudience::Staff)[0];
    let consumer = handle.local_addrs_for(ListenerAudience::Consumer)[0];
    assert_eq!(handle.local_addrs().len(), 2);
    Running {
        handle,
        staff,
        consumer,
        staff_hits,
        consumer_hits,
    }
}

fn consumer_router() -> Router {
    router_for(&[Audience::Consumer], services())
}

fn probed_entries() -> impl Iterator<Item = &'static spot_http::api::RouteEntry> {
    route_table().iter().filter(|e| {
        has_hand_wired_handler(e.operation)
            // These two need an optional service this harness does not wire.
            && !matches!(e.operation, "auth_permissions" | "field_reveal")
    })
}

#[tokio::test]
async fn the_consumer_listener_serves_no_table_route_and_the_staff_listener_serves_them() {
    let run = running(consumer_router()).await;
    for entry in probed_entries() {
        let path = sample(entry.path);
        let before = run.consumer_hits.load(Ordering::SeqCst);
        let (status, _) = request(run.consumer, entry.method, &path).await;
        assert_eq!(status, 404, "consumer {} {}", entry.method, entry.path);
        assert_eq!(run.consumer_hits.load(Ordering::SeqCst), before);

        let staff_before = run.staff_hits.load(Ordering::SeqCst);
        let (status, _) = request(run.staff, entry.method, &path).await;
        assert_ne!(status, 0);
        assert_eq!(
            run.staff_hits.load(Ordering::SeqCst),
            staff_before + 1,
            "staff {} {} did not reach the router",
            entry.method,
            entry.path
        );
    }
    // Path-confusion variants of a staff path are absent on the consumer listener too.
    for path in [
        "/api//v1/auth/session",
        "/%61pi/v1/auth/session",
        "/api/v1/auth/session/",
        "/api/v1/auth/../auth/session",
    ] {
        let (status, _) = request(run.consumer, "GET", path).await;
        assert_eq!(status, 404, "{path}");
    }
    assert_eq!(run.consumer_hits.load(Ordering::SeqCst), 0);
    drop(run.handle);
}

#[tokio::test]
async fn a_route_added_to_the_consumer_router_is_served_there_and_absent_on_staff() {
    let extra = Router::new().route("/api/v1/consumer-probe", get(|| async { "consumer" }));
    let run = running(consumer_router().merge(extra)).await;
    let (status, body) = request(run.consumer, "GET", "/api/v1/consumer-probe").await;
    assert_eq!((status, body.as_str()), (200, "consumer"));
    let (status, _) = request(run.staff, "GET", "/api/v1/consumer-probe").await;
    assert_eq!(status, 404);
    // The staff routes stay absent on the consumer listener with the extra route present.
    for entry in probed_entries() {
        let (status, _) = request(run.consumer, entry.method, &sample(entry.path)).await;
        assert_eq!(status, 404, "{} {}", entry.method, entry.path);
    }
    drop(run.handle);
}

#[tokio::test]
async fn site_routes_are_staff_only() {
    let run = running(consumer_router()).await;
    let (status, body) = request(run.staff, "GET", "/scim/v2/probe").await;
    assert_eq!((status, body.as_str()), (200, "scim"));
    let (status, _) = request(run.consumer, "GET", "/scim/v2/probe").await;
    assert_eq!(status, 404);
    drop(run.handle);
}

#[tokio::test]
async fn health_answers_on_both_and_the_consumer_body_has_no_check_names() {
    let run = running(consumer_router()).await;
    for addr in [run.staff, run.consumer] {
        let (status, body) = request(addr, "GET", "/api/health/live").await;
        assert_eq!(status, 200);
        assert!(body.contains("live"));
    }
    let (status, body) = request(run.staff, "GET", "/api/health/ready").await;
    assert_eq!(status, 200);
    assert!(body.contains("database"), "{body}");
    let (status, body) = request(run.consumer, "GET", "/api/health/ready").await;
    assert_eq!(status, 200);
    assert!(!body.contains("database"), "{body}");
    assert!(body.contains("ready"));
    drop(run.handle);
}
