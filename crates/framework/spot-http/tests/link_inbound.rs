//! The external-peer inbound route (`POST /api/v1/link/inbound/{peer}`, WP-CTL-11.T4): what the route
//! itself guarantees over a fake [`LinkInboundPort`]: one `401` for every refusal, the rate limit
//! answered before the peer is looked up, the body read under the cap and never parsed, the source
//! address handed over, and nothing mounted unless the host mounts it.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use hyper::body::Frame;
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::ports::link_inbound::{
    DEFAULT_MAX_BODY_BYTES, InboundCall, LinkInboundPort, LinkInboundRefusal,
};
use spot_http::api::router::routes::{self, RouteState, link_inbound};
use spot_http::api::service::BoxFuture;
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const PEER_CAP: usize = 64;

struct Fake {
    admits: AtomicUsize,
    /// Requests charged before the limit answers.
    budget: usize,
    #[allow(clippy::type_complexity)]
    received: Mutex<Vec<(String, Vec<u8>, Option<std::net::IpAddr>)>>,
}

impl Fake {
    fn new(budget: usize) -> Arc<Self> {
        Arc::new(Self {
            admits: AtomicUsize::new(0),
            budget,
            received: Mutex::new(Vec::new()),
        })
    }
}

impl LinkInboundPort for Fake {
    fn admit(&self, call: &InboundCall) -> Result<usize, LinkInboundRefusal> {
        if self.admits.fetch_add(1, Ordering::SeqCst) >= self.budget {
            return Err(LinkInboundRefusal::RateLimited);
        }
        if call.peer == "blocked" {
            return Err(LinkInboundRefusal::Rejected);
        }
        Ok(if call.peer == "acme" {
            PEER_CAP
        } else {
            DEFAULT_MAX_BODY_BYTES
        })
    }

    fn receive<'a>(
        &'a self,
        call: InboundCall,
        body: Bytes,
    ) -> BoxFuture<'a, Result<(), LinkInboundRefusal>> {
        Box::pin(async move {
            let good = call
                .headers
                .iter()
                .any(|(n, v)| n == "x-spot-signature" && v == "good");
            if call.peer == "acme" && good {
                self.received
                    .lock()
                    .unwrap()
                    .push((call.peer.clone(), body.to_vec(), call.source));
                Ok(())
            } else {
                Err(LinkInboundRefusal::Rejected)
            }
        })
    }
}

fn app(port: Arc<dyn LinkInboundPort>, mounted: bool) -> axum::Router {
    let acme = SiteId::new("acme").unwrap();
    let sites = StaticSites::new()
        .add(HOST, acme.clone())
        .add_site_name(acme);
    let mut builder = ServerBuilder::new(ServerConfig::default(), sites);
    if mounted {
        let mut ports = Ports::new();
        ports.provide::<dyn LinkInboundPort>(port);
        let state = RouteState::new(ports.subset(link_inbound::required_ports()).unwrap());
        builder = builder.site_routes(link_inbound::routes().with_state(state));
    }
    builder.build()
}

struct Reply {
    status: u16,
    content_type: Option<String>,
    cache: Option<String>,
    text: String,
}

async fn post(router: &axum::Router, path: &str, headers: &[(&str, &str)], body: Body) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, HOST);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(body).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 4_000))));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let cache = response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        content_type,
        cache,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn same(a: &Reply, b: &Reply) -> bool {
    (a.status, &a.content_type, &a.cache, &a.text) == (b.status, &b.content_type, &b.cache, &b.text)
}

#[tokio::test]
async fn a_signed_request_is_accepted_with_the_raw_body_and_the_source_address() {
    let port = Fake::new(100);
    let router = app(port.clone(), true);
    let body = br#"{"not":"parsed", "spacing" :  1}"#;
    let reply = post(
        &router,
        "/api/v1/link/inbound/acme",
        &[("x-spot-signature", "good")],
        Body::from(body.to_vec()),
    )
    .await;
    assert_eq!(reply.status, 202, "{}", reply.text);
    assert_eq!(reply.text, r#"{"accepted":true}"#);
    assert_eq!(reply.cache.as_deref(), Some("no-store"));
    let got = port.received.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, body.to_vec(), "the port gets the raw bytes");
    assert_eq!(got[0].2, Some("203.0.113.9".parse().unwrap()));
}

#[tokio::test]
async fn every_refusal_is_the_same_401_and_never_says_whether_the_peer_exists() {
    let router = app(Fake::new(100), true);
    let bad_signature = post(
        &router,
        "/api/v1/link/inbound/acme",
        &[("x-spot-signature", "bad")],
        Body::from("{}"),
    )
    .await;
    let unknown = post(
        &router,
        "/api/v1/link/inbound/nobody",
        &[("x-spot-signature", "good")],
        Body::from("{}"),
    )
    .await;
    let blocked = post(
        &router,
        "/api/v1/link/inbound/blocked",
        &[("x-spot-signature", "good")],
        Body::from("{}"),
    )
    .await;
    let malformed = post(
        &router,
        "/api/v1/link/inbound/Not_A.Peer",
        &[("x-spot-signature", "good")],
        Body::from("{}"),
    )
    .await;
    let oversize_known = post(
        &router,
        "/api/v1/link/inbound/acme",
        &[("x-spot-signature", "good")],
        Body::from(vec![b'x'; PEER_CAP + 1]),
    )
    .await;
    let oversize_unknown = post(
        &router,
        "/api/v1/link/inbound/nobody",
        &[("x-spot-signature", "good")],
        Body::from(vec![b'x'; PEER_CAP + 1]),
    )
    .await;
    assert_eq!(bad_signature.status, 401);
    assert_eq!(
        bad_signature.text,
        r#"{"error":{"code":"link_inbound.unauthorized"}}"#
    );
    for other in [&unknown, &blocked, &malformed, &oversize_known] {
        assert!(
            same(&bad_signature, other),
            "{} vs {}",
            other.status,
            other.text
        );
    }
    // An unknown peer with the same oversize body: refused alike (here it is simply not a peer).
    assert_eq!(oversize_unknown.status, 401);
    assert!(same(&bad_signature, &oversize_unknown));
}

#[tokio::test]
async fn the_rate_limit_answers_before_the_peer_is_looked_up() {
    let router = app(Fake::new(2), true);
    for _ in 0..2 {
        post(
            &router,
            "/api/v1/link/inbound/acme",
            &[("x-spot-signature", "good")],
            Body::from("{}"),
        )
        .await;
    }
    let known = post(
        &router,
        "/api/v1/link/inbound/acme",
        &[("x-spot-signature", "good")],
        Body::from("{}"),
    )
    .await;
    let unknown = post(
        &router,
        "/api/v1/link/inbound/nobody",
        &[],
        Body::from("{}"),
    )
    .await;
    assert_eq!(known.status, 429);
    assert_eq!(
        known.text, r#"{"error":{"code":"api.rate_limited"}}"#,
        "the contract reuses api.rate_limited"
    );
    assert!(
        same(&known, &unknown),
        "a known and an unknown peer are limited alike"
    );
}

/// A body that never ends, counting what the reader pulled.
struct Endless {
    pulled: Arc<AtomicUsize>,
}

impl hyper::body::Body for Endless {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.pulled.fetch_add(16, Ordering::SeqCst);
        Poll::Ready(Some(Ok(Frame::data(Bytes::from(vec![b'x'; 16])))))
    }
}

#[tokio::test]
async fn an_endless_body_is_cut_at_the_peers_cap() {
    let router = app(Fake::new(100), true);
    let pulled = Arc::new(AtomicUsize::new(0));
    let reply = post(
        &router,
        "/api/v1/link/inbound/acme",
        &[("x-spot-signature", "good")],
        Body::new(Endless {
            pulled: pulled.clone(),
        }),
    )
    .await;
    assert_eq!(reply.status, 401);
    assert!(
        pulled.load(Ordering::SeqCst) <= PEER_CAP + 64,
        "read stopped near the cap"
    );
}

#[tokio::test]
async fn a_declared_length_over_the_cap_is_refused_before_the_body_is_read() {
    let router = app(Fake::new(100), true);
    let reply = post(
        &router,
        "/api/v1/link/inbound/acme",
        &[("x-spot-signature", "good"), ("content-length", "100000")],
        Body::from("{}"),
    )
    .await;
    assert_eq!(reply.status, 401);
}

#[tokio::test]
async fn nothing_is_mounted_unless_the_service_provides_the_port() {
    let router = app(Fake::new(100), false);
    let reply = post(
        &router,
        "/api/v1/link/inbound/acme",
        &[("x-spot-signature", "good")],
        Body::from("{}"),
    )
    .await;
    assert_eq!(reply.status, 404);
}

fn registration() -> routes::Registration {
    routes::Registration {
        name: "link_inbound",
        serves: link_inbound::SERVES,
        required_ports: link_inbound::required_ports,
        routes: link_inbound::routes,
        public: Some(link_inbound::PUBLIC),
    }
}

#[test]
fn the_registry_mounts_the_group_as_public_only_with_its_port() {
    assert_eq!(link_inbound::SERVES, &["link_inbound"]);
    let mounted = routes::mount(&[registration()], &Ports::new(), &[]);
    assert!(mounted.report.mounted.is_empty());
    let mut ports = Ports::new();
    ports.provide::<dyn LinkInboundPort>(Fake::new(10));
    let mounted = routes::mount(&[registration()], &ports, &[]);
    assert_eq!(mounted.report.mounted, vec!["link_inbound"]);
    assert_eq!(mounted.report.public, vec!["link_inbound"]);
    assert_eq!(mounted.public[0].0, routes::RateClass::Auth);
}
