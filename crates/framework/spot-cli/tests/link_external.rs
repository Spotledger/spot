//! External peers hosted (WP-CTL-11.T4): the outbound pass through the egress policy and the
//! inbound webhook route, over the real exchange stores in memory and the real route. The
//! fixture (peer, vault, signed requests, a fake peer channel) is the one of spot-link-external's
//! own tests, included by path.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    missing_docs,
    dead_code
)]

#[path = "../../spot-link-external/tests/common/mod.rs"]
mod common;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, header};
use common::*;
use spot_cli::services::link_exchange::SendHold;
use spot_cli::services::link_external::{
    ConfiguredPeer, ExternalInbound, ExternalPeers, ExternalService, FileNonceStore,
    LinkExternalPorts, NonceStore, Settings, register,
};
use spot_core::{SiteId, SystemClock};
use spot_http::api::ports::link_inbound::LinkInboundPort;
use spot_http::api::router::routes::link_inbound;
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_link_exchange::{
    InboxConfig, Lifetimes, MemoryInbox, MemoryOutbox, MemoryQuarantine, MessageState,
    OutboxConfig, Retention, Submit,
};
use spot_link_external::{ExternalPeer, PeerChannel, WebhookChannel};
use spot_link_transport::LinkHost;
use spot_tenant_link::{
    ExchangePayload, FieldName, FieldValue, Link, LinkStore, MemoryAuditLog, MemoryLinkStore,
    SeedSigner,
};
use tower::ServiceExt as _;

const SITE_HOST: &str = "erp.acme.com";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_file(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "spot-link-external-{}-{tag}-{n}",
        std::process::id()
    ))
}

struct Hold(AtomicBool);

impl SendHold for Hold {
    fn held(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct Peers(Vec<Arc<ConfiguredPeer>>);

impl ExternalPeers for Peers {
    fn peers(&self) -> Vec<Arc<ConfiguredPeer>> {
        self.0.clone()
    }
}

/// An egress whose policy allows nothing: what a host not on the allow-list gets.
struct Denying;

#[async_trait]
impl spot_webhooks::Egress for Denying {
    async fn post(
        &self,
        _tenant: &str,
        _url: &url::Url,
        _headers: http::HeaderMap,
        _body: Vec<u8>,
    ) -> Result<spot_egress::Response, spot_egress::EgressError> {
        Err(spot_egress::EgressError::PolicyDenied)
    }
}

struct Rig {
    host: LinkHost,
    link: Link,
    outbox: Arc<MemoryOutbox>,
    inbox: Arc<MemoryInbox>,
    audit: Arc<MemoryAuditLog>,
    hold: Arc<Hold>,
    ports: Arc<LinkExternalPorts>,
    router: axum::Router,
}

fn rig(
    channel: Arc<dyn PeerChannel>,
    tweak: impl FnOnce(&mut ExternalPeer),
    allowed_sources: Vec<IpAddr>,
    nonce_path: &Path,
) -> Rig {
    let world = World::new();
    let mut peer = world.peer.clone();
    tweak(&mut peer);
    let mut link = world.link.clone();
    let tenant = world.tenant.clone();
    let links = Arc::new(MemoryLinkStore::new());
    // A new link is inserted unversioned; the store answers with the stored one.
    link.version = 0;
    let link = links.insert(link).unwrap();
    let outbox = Arc::new(MemoryOutbox::new());
    let inbox = Arc::new(MemoryInbox::new());
    let audit = Arc::new(MemoryAuditLog::new());
    let host = LinkHost {
        links,
        outbox: outbox.clone(),
        inbox: inbox.clone(),
        audit: audit.clone(),
        quarantine: Arc::new(MemoryQuarantine::new()),
        signer: Arc::new(SeedSigner::from_seed([1u8; 32], tenant)),
        ids: Arc::new(Counter::new()),
        lifetimes: Lifetimes::new(Retention::from_days(3).unwrap()),
        inbox_config: InboxConfig::default(),
        outbox_config: OutboxConfig {
            resend_after_ms: 0,
            unapplied_resend_after_ms: u64::MAX,
        },
        clock: Arc::new(SystemClock::new()),
        region: None,
    };
    let configured = Arc::new(ConfiguredPeer {
        peer,
        link: link.id,
        bridge: Arc::new(SeedSigner::for_external_peer([9u8; 32], "acme-erp").unwrap()),
        allowed_sources,
    });
    let hold = Arc::new(Hold(AtomicBool::new(false)));
    let nonces: Arc<dyn NonceStore> =
        Arc::new(FileNonceStore::open(nonce_path, 1_000, host.now_ms().unwrap()).unwrap());
    let ports = Arc::new(LinkExternalPorts {
        host: host.clone(),
        peers: Arc::new(Peers(vec![configured])),
        channel,
        vault: vault(),
        nonces,
        hold: hold.clone(),
    });
    let inbound: Arc<dyn LinkInboundPort> = Arc::new(ExternalInbound::new(
        Arc::clone(&ports),
        &Settings::default(),
    ));
    let acme = SiteId::new("acme").unwrap();
    let sites = StaticSites::new()
        .add(SITE_HOST, acme.clone())
        .add_site_name(acme);
    let mut api_ports = spot_http::api::Ports::new();
    api_ports.provide::<dyn LinkInboundPort>(inbound);
    let state = spot_http::api::router::routes::RouteState::new(
        api_ports.subset(link_inbound::required_ports()).unwrap(),
    );
    let router = ServerBuilder::new(ServerConfig::default(), sites)
        .site_routes(link_inbound::routes().with_state(state))
        .build();
    Rig {
        host,
        link,
        outbox,
        inbox,
        audit,
        hold,
        ports,
        router,
    }
}

fn plain_rig(nonce_path: &Path) -> Rig {
    rig(
        Arc::new(FakePeer::answering(200)),
        |_| {},
        Vec::new(),
        nonce_path,
    )
}

fn enqueue(r: &Rig, text: &str) {
    let conversation = conversation("order-1");
    let topic = topic();
    let mut payload = ExchangePayload::new();
    payload.insert(
        FieldName::new("message.body").unwrap(),
        FieldValue::Text(text.to_owned()),
    );
    let now = r.host.now_ms().unwrap();
    r.host
        .outbox()
        .enqueue_message(
            &Submit {
                link: &r.link,
                conversation: &conversation,
                topic: &topic,
                schema_version: 1,
                now,
                actor: "user:alice",
                oracle: None,
            },
            &payload,
        )
        .unwrap();
}

fn states(r: &Rig) -> Vec<MessageState> {
    r.outbox.all().unwrap().iter().map(|e| e.state).collect()
}

struct Reply {
    status: u16,
    body: String,
}

async fn post(r: &Rig, name: &str, headers: Vec<(String, String)>, body: Vec<u8>) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/link/inbound/{name}"))
        .header(header::HOST, SITE_HOST);
    for (key, value) in headers {
        builder = builder.header(key, value);
    }
    let mut request = builder.body(Body::from(body)).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 4_000))));
    let response = r.router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Reply {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// A request signed as the peer would, at `at_secs`.
fn signed_headers(body: &str, at_secs: i64, secret: &str) -> Vec<(String, String)> {
    let signature =
        spot_webhooks::SigningKey::new(secret.as_bytes()).sign(at_secs, body.as_bytes());
    vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        (
            "x-spot-signature".to_owned(),
            format!("t={at_secs},v1={signature}"),
        ),
    ]
}

fn now_secs(r: &Rig) -> i64 {
    i64::try_from(r.host.now_ms().unwrap() / 1_000).unwrap()
}

#[test]
fn settings_are_off_by_default_and_unknown_keys_refused() {
    assert!(!Settings::default().enabled);
    assert!(
        Settings {
            pump_batch: 0,
            ..Settings::default()
        }
        .validate()
        .is_err()
    );
    assert!(toml::from_str::<Settings>("enabled = true\nnope = 1").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_signature_is_accepted_once_and_a_replay_is_refused() {
    let file = temp_file("replay");
    let r = plain_rig(&file);
    let body = message_body("PO-1", "order-1", "hello");
    let headers = signed_headers(&body, now_secs(&r), SECRET);
    let first = post(&r, "acme-erp", headers.clone(), body.clone().into_bytes()).await;
    assert_eq!(first.status, 202, "{}", first.body);
    assert_eq!(r.inbox.stored().unwrap().len(), 1);
    let replay = post(&r, "acme-erp", headers, body.into_bytes()).await;
    assert_eq!(replay.status, 401);
    assert_eq!(
        r.inbox.stored().unwrap().len(),
        1,
        "a replay stores nothing"
    );
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_is_refused_after_a_restart() {
    let file = temp_file("restart");
    let body = message_body("PO-2", "order-1", "hello");
    let first = plain_rig(&file);
    let headers = signed_headers(&body, now_secs(&first), SECRET);
    assert_eq!(
        post(
            &first,
            "acme-erp",
            headers.clone(),
            body.clone().into_bytes()
        )
        .await
        .status,
        202
    );
    drop(first);
    // A new process: new stores, the same durable nonce file.
    let second = plain_rig(&file);
    let again = post(&second, "acme-erp", headers, body.into_bytes()).await;
    assert_eq!(again.status, 401);
    assert_eq!(second.inbox.stored().unwrap().len(), 0);
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_timestamp_is_refused() {
    let file = temp_file("stale");
    let r = plain_rig(&file);
    let body = message_body("PO-3", "order-1", "hello");
    let headers = signed_headers(&body, now_secs(&r) - 3_600, SECRET);
    assert_eq!(
        post(&r, "acme-erp", headers, body.into_bytes())
            .await
            .status,
        401
    );
    assert_eq!(r.inbox.stored().unwrap().len(), 0);
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_peer_is_indistinguishable_from_an_unknown_one() {
    let file = temp_file("unknown");
    let r = plain_rig(&file);
    let body = message_body("PO-4", "order-1", "hello");
    let good = signed_headers(&body, now_secs(&r), SECRET);
    let wrong_secret = signed_headers(&body, now_secs(&r), "some-other-secret");
    let bad = post(&r, "acme-erp", wrong_secret, body.clone().into_bytes()).await;
    let unknown = post(&r, "nobody-here", good, body.into_bytes()).await;
    assert_eq!(bad.status, 401);
    assert_eq!(unknown.status, 401);
    assert_eq!(bad.body, unknown.body);
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversize_body_is_refused() {
    let file = temp_file("oversize");
    let r = rig(
        Arc::new(FakePeer::answering(200)),
        |p| p.max_body_bytes = 64,
        Vec::new(),
        &file,
    );
    let body = message_body("PO-5", "order-1", &"x".repeat(200));
    let headers = signed_headers(&body, now_secs(&r), SECRET);
    assert_eq!(
        post(&r, "acme-erp", headers, body.into_bytes())
            .await
            .status,
        401
    );
    assert_eq!(r.inbox.stored().unwrap().len(), 0);
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_granted_fields_are_accepted() {
    let file = temp_file("fields");
    let r = plain_rig(&file);
    let body = r#"{"id":"PO-6","conversation":"order-1","topic":"bpm.message","schema_version":1,"payload":{"message.body":"x","message.secret":"y"}}"#;
    let headers = signed_headers(body, now_secs(&r), SECRET);
    let reply = post(&r, "acme-erp", headers, body.as_bytes().to_vec()).await;
    assert_eq!(reply.status, 401, "a field outside the grant is refused");
    assert!(
        r.inbox
            .stored()
            .unwrap()
            .iter()
            .all(|(_, e)| !e.message.payload.contains("secret")),
        "nothing outside the grant is stored"
    );
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_outside_the_allow_list_is_refused() {
    let file = temp_file("allow");
    let allowed: IpAddr = "198.51.100.7".parse().unwrap();
    let r = rig(
        Arc::new(FakePeer::answering(200)),
        |_| {},
        vec![allowed],
        &file,
    );
    let body = message_body("PO-7", "order-1", "hello");
    let headers = signed_headers(&body, now_secs(&r), SECRET);
    // The request comes from 203.0.113.9.
    assert_eq!(
        post(&r, "acme-erp", headers, body.into_bytes())
            .await
            .status,
        401
    );
    assert_eq!(r.inbox.stored().unwrap().len(), 0);
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rate_limit_applies_per_peer_name() {
    let file = temp_file("rate");
    let r = plain_rig(&file);
    let mut last = 0;
    for _ in 0..200 {
        last = post(&r, "acme-erp", Vec::new(), Vec::new()).await.status;
        if last == 429 {
            break;
        }
    }
    assert_eq!(last, 429);
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outbound_message_to_a_host_the_policy_denies_is_not_delivered() {
    let file = temp_file("egress");
    let channel = Arc::new(WebhookChannel::new(Arc::new(Denying), vault(), "tenant-1"));
    let r = rig(channel, |_| {}, Vec::new(), &file);
    enqueue(&r, "hello");
    let service = ExternalService::new(Arc::clone(&r.ports), Settings::default());
    assert_eq!(service.pass().await.unwrap(), 1);
    assert!(
        states(&r)
            .iter()
            .all(|s| !matches!(s, MessageState::Acknowledged | MessageState::Applied)),
        "{:?}",
        states(&r)
    );
    assert!(
        r.audit
            .entries()
            .unwrap()
            .iter()
            .any(|e| e.kind == "tenant_link.refused"),
        "the failed delivery is audited"
    );
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outbound_message_is_delivered_through_the_channel_and_the_hold_suspends_it() {
    let file = temp_file("outbound");
    let r = plain_rig(&file);
    enqueue(&r, "hello");
    r.hold.0.store(true, Ordering::SeqCst);
    let service = ExternalService::new(Arc::clone(&r.ports), Settings::default());
    assert_eq!(service.pass().await.unwrap(), 0);
    assert_eq!(states(&r), vec![MessageState::Queued]);
    r.hold.0.store(false, Ordering::SeqCst);
    assert_eq!(service.pass().await.unwrap(), 1);
    assert_eq!(states(&r), vec![MessageState::Acknowledged]);
    let _ = std::fs::remove_file(&file);
}

#[test]
fn the_file_nonce_store_claims_once_and_survives_a_reopen() {
    let file = temp_file("nonce");
    let store = FileNonceStore::open(&file, 10, 1_000).unwrap();
    assert!(store.claim("p", "n1", 1_000, 5_000).unwrap());
    assert!(!store.claim("p", "n1", 1_100, 5_000).unwrap());
    assert!(
        store.claim("q", "n1", 1_100, 5_000).unwrap(),
        "claims are per peer"
    );
    drop(store);
    let reopened = FileNonceStore::open(&file, 10, 2_000).unwrap();
    assert!(!reopened.claim("p", "n1", 2_000, 6_000).unwrap());
    // After expiry the nonce is free again, and the table refuses to grow past its bound.
    assert!(reopened.claim("p", "n1", 5_001, 9_000).unwrap());
    let small = FileNonceStore::open(&temp_file("nonce-small"), 1, 0).unwrap();
    assert!(small.claim("p", "a", 0, 10).unwrap());
    assert!(
        small.claim("p", "b", 1, 10).is_err(),
        "a full table refuses"
    );
    assert!(file.exists());
    let _ = std::fs::remove_file(&file);
}

#[test]
fn enabled_without_ports_refuses_to_start() {
    use std::collections::BTreeMap;
    let mut settings = BTreeMap::new();
    settings.insert(
        "link_external".to_owned(),
        "enabled = true".parse::<toml::Value>().unwrap(),
    );
    let mut ctx = spot_cli::services::SiteCtx::new(
        "site",
        std::time::Duration::from_secs(1),
        Arc::new(SystemClock::new()),
        settings,
        true,
    );
    let error = register(&mut ctx).unwrap_err();
    assert!(error.to_string().contains("LinkExternalPorts"), "{error}");
    let mut off = spot_cli::services::SiteCtx::new(
        "site",
        std::time::Duration::from_secs(1),
        Arc::new(SystemClock::new()),
        BTreeMap::new(),
        true,
    );
    register(&mut off).unwrap();
}
