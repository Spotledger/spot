//! The cluster-redis profile hosting (WP-PLT-19.T2).
//!
//! The settings, the secret reference, the fallback bus and the registration run everywhere.
//! The tests that need a server read `SPOT_TEST_REDIS_URL` (for example
//! `redis://127.0.0.1:6391`) and print `SKIPPED` with the reason when it is unset; the ACL tests
//! also read `SPOT_TEST_REDIS_ACL_URL` (a server with a restricted user `spot`, password
//! `testpw`, default user off).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use spot_cache::CacheStore;
use spot_cli::services::SiteCtx;
use spot_cli::services::cluster_redis::{
    AclVerdict, FallbackBus, PasswordRef, Settings, Status, build, register, validate,
};
use spot_coordinator::Coordinator;
use spot_coordinator::error::{LEASE_HELD, LEASE_LOST};
use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use spot_meta_live::{
    BusEvent, BusFuture, InProcessBus, Invalidation, InvalidationBus, InvalidationStream, MetaHash,
    Version, next_event,
};
use time::OffsetDateTime;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

// ---- helpers ------------------------------------------------------------------------------

fn base() -> Option<String> {
    let url = std::env::var("SPOT_TEST_REDIS_URL").ok();
    if url.is_none() {
        eprintln!("SKIPPED: SPOT_TEST_REDIS_URL is not set (needs a Redis or Valkey server)");
    }
    url
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A prefix no other test or run shares, so suites can use one server together.
fn unique_prefix(tag: &str) -> String {
    format!(
        "t2:{}:{}:{tag}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

fn enabled(url: &str, prefix: &str) -> Settings {
    Settings {
        enabled: true,
        url: Some(url.to_owned()),
        allow_plaintext: true,
        prefix: prefix.to_owned(),
        ..Settings::default()
    }
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

fn system() -> Arc<dyn Clock> {
    Arc::new(SystemClock::new())
}

fn invalidation(version: u64) -> Invalidation {
    Invalidation::meta(
        Version::new(version),
        MetaHash::from_hex(format!("{version:064x}")),
    )
}

async fn put(cache: &Arc<dyn CacheStore>, site: &SiteId, key: &str, value: &[u8]) {
    cache.set(site, key, Arc::from(value), None).await.unwrap();
}

async fn read(cache: &Arc<dyn CacheStore>, site: &SiteId, key: &str) -> Option<Vec<u8>> {
    cache
        .get(site, key)
        .await
        .unwrap()
        .map(|bytes| bytes.to_vec())
}

async fn heard(stream: &mut InvalidationStream) -> Option<BusEvent> {
    tokio::time::timeout(Duration::from_secs(5), next_event(stream))
        .await
        .ok()
        .flatten()
}

fn is_change(event: Option<BusEvent>, version: u64) -> bool {
    matches!(event, Some(BusEvent::Change(i)) if i.version == Version::new(version))
}

/// A TCP proxy to the server that can be cut and restored on the same port: a lost server.
struct Proxy {
    addr: SocketAddr,
    target: String,
    task: Option<JoinHandle<()>>,
}

impl Proxy {
    async fn start(target: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut proxy = Self {
            addr,
            target: target.to_owned(),
            task: None,
        };
        proxy.serve(listener);
        proxy
    }

    fn serve(&mut self, listener: TcpListener) {
        let target = self.target.clone();
        self.task = Some(tokio::spawn(async move {
            // Aborting the accept task drops the set, which aborts every open copy: a cut.
            let mut copies = tokio::task::JoinSet::new();
            while let Ok((mut client, _)) = listener.accept().await {
                let target = target.clone();
                copies.spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(&target).await {
                        let _ = copy_bidirectional(&mut client, &mut server).await;
                    }
                });
            }
        }));
    }

    fn url(&self) -> String {
        format!("redis://{}", self.addr)
    }
}

// ---- settings and secret references (no server) -----------------------------------------

fn bad(settings: &Settings) -> String {
    validate(settings).unwrap_err().to_string()
}

#[test]
fn plaintext_is_refused_without_the_flag_and_off_loopback() {
    let mut s = enabled("redis://127.0.0.1:6379", "p");
    s.allow_plaintext = false;
    assert!(bad(&s).contains("rediss://"), "{}", bad(&s));
    s.allow_plaintext = true;
    assert!(validate(&s).is_ok());
    for url in ["redis://localhost:6379", "redis://[::1]:6379"] {
        assert!(validate(&enabled(url, "p")).is_ok(), "{url}");
    }
    let remote = enabled("redis://10.1.2.3:6379", "p");
    assert!(bad(&remote).contains("loopback"), "{}", bad(&remote));
    // TLS needs no flag.
    let mut tls = enabled("rediss://redis.internal:6380", "p");
    tls.allow_plaintext = false;
    assert!(validate(&tls).is_ok());
}

#[test]
fn a_url_cannot_carry_a_credential_or_turn_verification_off() {
    for url in [
        "rediss://user:hunter2@redis.internal:6380",
        "rediss://:hunter2@redis.internal:6380",
        "rediss://redis.internal:6380/#insecure",
        "rediss://redis.internal:6380/?protocol=3",
    ] {
        let message = bad(&enabled(url, "p"));
        assert!(!message.contains("hunter2"), "{message}");
        assert!(message.contains("url"), "{message}");
    }
    let mut s = enabled("rediss://redis.internal:6380", "p");
    s.url = None;
    assert!(bad(&s).contains("url"));
}

#[test]
fn the_other_settings_are_checked() {
    for prefix in ["", "has space", ":lead", "a/b", &"x".repeat(65)] {
        assert!(bad(&enabled("redis://127.0.0.1:1", prefix)).contains("prefix"));
    }
    let mut s = enabled("redis://127.0.0.1:1", "p");
    s.pool = 4;
    assert!(bad(&s).contains("pool"));
    s.pool = 1;
    s.password_ref = Some("hunter2".to_owned());
    let message = bad(&s);
    assert!(
        message.contains("password_ref") && !message.contains("hunter2"),
        "{message}"
    );
    s.password_ref = None;
    s.max_clock_skew_ms = 0;
    assert!(validate(&s).is_err());
}

#[test]
fn a_password_comes_from_a_reference_never_a_value() {
    assert_eq!(
        PasswordRef::parse("env:SPOT_REDIS_PW").unwrap(),
        PasswordRef::Env("SPOT_REDIS_PW".into())
    );
    for text in [
        "env:lower",
        "env:",
        "env:1BAD",
        "file:relative/path",
        "hunter2",
        "vault:x",
    ] {
        assert!(PasswordRef::parse(text).is_err(), "{text}");
    }
    // `PATH` exists in every test environment; the other name is never set.
    assert!(PasswordRef::parse("env:PATH").unwrap().resolve().is_ok());
    let missing = PasswordRef::parse("env:SPOT_T2_SURELY_NOT_SET")
        .unwrap()
        .resolve();
    assert!(
        missing
            .unwrap_err()
            .to_string()
            .contains("SPOT_T2_SURELY_NOT_SET")
    );
}

#[cfg(unix)]
#[test]
fn a_password_file_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join(format!("redis-pw-{}", std::process::id()));
    std::fs::write(&path, "s3cret-value\n").unwrap();
    let reference = PasswordRef::parse(&format!("file:{}", path.display())).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let message = reference.resolve().unwrap_err().to_string();
    assert!(
        message.contains("private") && !message.contains("s3cret-value"),
        "{message}"
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let secret = reference.resolve().unwrap();
    assert_eq!(format!("{secret:?}"), "Secret([redacted])");
    std::fs::write(&path, "\n").unwrap();
    assert!(
        reference
            .resolve()
            .unwrap_err()
            .to_string()
            .contains("empty")
    );
    let _ = std::fs::remove_file(&path);
}

// ---- registration (no server) ------------------------------------------------------------

fn ctx(table: Option<&str>, assembling: bool) -> SiteCtx<'static> {
    let mut settings = BTreeMap::new();
    if let Some(text) = table {
        settings.insert("cluster_redis".to_owned(), toml::from_str(text).unwrap());
    }
    SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
        settings,
        assembling,
    )
}

#[tokio::test]
async fn a_disabled_or_absent_table_registers_nothing() {
    for table in [None, Some("enabled = false\nurl = \"redis://127.0.0.1:1\"")] {
        let mut ctx = ctx(table, true);
        register(&mut ctx).await.unwrap();
        assert!(ctx.ports().is_empty());
        assert!(ctx.port::<dyn Coordinator>().is_none());
    }
}

#[tokio::test]
async fn an_enabled_table_with_a_bad_setting_stops_the_start() {
    let mut ctx = ctx(
        Some("enabled = true\nurl = \"redis://10.9.9.9:6379\"\nallow_plaintext = true"),
        true,
    );
    let error = register(&mut ctx).await.unwrap_err().to_string();
    assert!(error.contains("loopback"), "{error}");
    let mut typo = ctx_with_unknown_key();
    assert!(register(&mut typo).await.is_err());
}

fn ctx_with_unknown_key() -> SiteCtx<'static> {
    ctx(
        Some("enabled = true\nurl = \"rediss://r:1\"\nunknown_key = 1"),
        true,
    )
}

#[tokio::test]
async fn an_unreachable_server_stops_the_start_and_never_falls_back() {
    // Nothing listens on port 1: the site that asked for Redis must not start without it.
    let settings = enabled("redis://127.0.0.1:1", &unique_prefix("down"));
    let error = build(&settings, &site("acme"), system())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("cannot connect"), "{error}");
}

// ---- the fallback bus (no server) ---------------------------------------------------------

/// A primary bus that refuses every publish and never delivers.
struct Refusing;

impl InvalidationBus for Refusing {
    fn publish<'a>(&'a self, _: &'a SiteId, _: Invalidation) -> BusFuture<'a, ()> {
        Box::pin(async {
            Err(spot_meta_live::LiveError::Store {
                message: "server lost".into(),
            })
        })
    }

    fn subscribe(&self, _: &SiteId) -> InvalidationStream {
        Box::pin(futures_pending())
    }
}

fn futures_pending() -> impl futures_core::Stream<Item = BusEvent> + Send {
    struct Pending;
    impl futures_core::Stream for Pending {
        type Item = BusEvent;
        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<BusEvent>> {
            std::task::Poll::Pending
        }
    }
    Pending
}

#[tokio::test]
async fn a_refused_publish_falls_back_to_the_per_node_bus_and_says_so() {
    let status = status();
    let bus = FallbackBus::new(Arc::new(Refusing), Arc::clone(&status), system());
    let acme = site("acme");
    let mut stream = bus.subscribe(&acme);
    bus.publish(&acme, invalidation(7)).await.unwrap();
    assert!(is_change(heard(&mut stream).await, 7));
    assert!(status.bus_degraded());
    assert_eq!(status.bus_fallbacks(), 1);
    assert!(status.degraded_reason().unwrap().contains("per-node bus"));
}

#[tokio::test]
async fn a_working_primary_delivers_each_change_once() {
    let status = status();
    let primary: Arc<dyn InvalidationBus> = Arc::new(InProcessBus::new(16));
    let bus = FallbackBus::new(primary, Arc::clone(&status), system());
    let acme = site("acme");
    let mut stream = bus.subscribe(&acme);
    bus.publish(&acme, invalidation(1)).await.unwrap();
    bus.publish(&acme, invalidation(2)).await.unwrap();
    assert!(is_change(heard(&mut stream).await, 1));
    assert!(is_change(heard(&mut stream).await, 2));
    // Nothing else arrives: no duplicate from a second path.
    let extra = tokio::time::timeout(Duration::from_millis(200), next_event(&mut stream)).await;
    assert!(extra.is_err());
    assert!(!status.bus_degraded());
    assert_eq!(status.degraded_reason(), None);
}

fn status() -> Arc<Status> {
    Status::new()
}

// ---- against a server --------------------------------------------------------------------

#[tokio::test]
async fn two_hosts_share_leases_and_a_stale_holder_is_refused() {
    let Some(url) = base() else { return };
    let prefix = unique_prefix("lease");
    let acme = site("acme");
    let a = build(&enabled(&url, &prefix), &acme, system())
        .await
        .unwrap();
    let b = build(&enabled(&url, &prefix), &acme, system())
        .await
        .unwrap();
    let name = "scheduler:acme";
    let ttl = Duration::from_secs(30);

    let first = a.coordinator.acquire(name, "node-a", ttl).await.unwrap();
    // Only one host runs the scheduler.
    assert_eq!(
        b.coordinator
            .acquire(name, "node-b", ttl)
            .await
            .unwrap_err()
            .code,
        LEASE_HELD
    );
    a.coordinator.check_fence(name, first.token).await.unwrap();
    a.coordinator.release(&first).await.unwrap();

    let second = b.coordinator.acquire(name, "node-b", ttl).await.unwrap();
    assert!(second.token.0 > first.token.0, "fencing tokens only grow");
    // The first holder is stale: it cannot renew, and its token fails the fence.
    assert_eq!(
        a.coordinator.renew(&first, ttl).await.unwrap_err().code,
        LEASE_LOST
    );
    assert!(a.coordinator.check_fence(name, first.token).await.is_err());
    b.coordinator.release(&second).await.unwrap();
}

#[tokio::test]
async fn an_invalidation_reaches_both_hosts() {
    let Some(url) = base() else { return };
    let prefix = unique_prefix("bus");
    let acme = site("acme");
    let a = build(&enabled(&url, &prefix), &acme, system())
        .await
        .unwrap();
    let b = build(&enabled(&url, &prefix), &acme, system())
        .await
        .unwrap();
    let mut on_a = a.bus.subscribe(&acme);
    let mut on_b = b.bus.subscribe(&acme);
    // The readers connect in the background; give the subscriptions a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.bus.publish(&acme, invalidation(9)).await.unwrap();
    assert!(is_change(heard(&mut on_a).await, 9));
    assert!(is_change(heard(&mut on_b).await, 9));
    // Another site on the same prefix hears nothing.
    let mut other = a.bus.subscribe(&site("globex"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.bus.publish(&acme, invalidation(10)).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(400), next_event(&mut other))
            .await
            .is_err()
    );
    assert!(!a.status.bus_degraded());
}

#[tokio::test]
async fn tenants_never_share_a_cache_key() {
    let Some(url) = base() else { return };
    let prefix = unique_prefix("tenant");
    let (acme, globex) = (site("acme"), site("globex"));
    let host = build(&enabled(&url, &prefix), &acme, system())
        .await
        .unwrap();
    put(&host.cache, &acme, "price", b"acme").await;
    put(&host.cache, &globex, "price", b"globex").await;
    assert_eq!(
        read(&host.cache, &acme, "price").await,
        Some(b"acme".to_vec())
    );
    assert_eq!(
        read(&host.cache, &globex, "price").await,
        Some(b"globex".to_vec())
    );
    host.cache.clear_site(&acme).await;
    assert_eq!(read(&host.cache, &acme, "price").await, None);
    assert_eq!(
        read(&host.cache, &globex, "price").await,
        Some(b"globex".to_vec())
    );
    host.cache.clear_site(&globex).await;
}

#[tokio::test]
async fn losing_the_server_stops_claims_and_recovery_never_double_runs() {
    let Some(url) = base() else { return };
    let target = url.trim_start_matches("redis://").to_owned();
    let mut proxy = Proxy::start(&target).await;
    let prefix = unique_prefix("kill");
    let acme = site("acme");
    // Node A reaches the server through the proxy; node B directly.
    let a = build(&enabled(&proxy.url(), &prefix), &acme, system())
        .await
        .unwrap();
    let b = build(&enabled(&url, &prefix), &acme, system())
        .await
        .unwrap();
    let name = "serve:acme:jobs";
    let ttl = Duration::from_secs(2);

    let held = a.coordinator.acquire(name, "node-a", ttl).await.unwrap();
    put(&a.cache, &acme, "warm", b"before").await;
    assert_eq!(
        read(&a.cache, &acme, "warm").await,
        Some(b"before".to_vec())
    );

    // The server is lost to node A.
    proxy.cut();
    let renewed = a.coordinator.renew(&held, ttl).await;
    assert_eq!(
        renewed.unwrap_err().code,
        "coordinator.unavailable",
        "fail closed"
    );
    assert!(
        a.coordinator
            .acquire("serve:acme:other", "node-a", ttl)
            .await
            .is_err()
    );
    assert_eq!(
        read(&a.cache, &acme, "warm").await,
        None,
        "the cache degrades to a miss"
    );
    a.service.probe_once().await;
    assert!(!a.status.connected());
    assert!(a.status.degraded_reason().unwrap().contains("unreachable"));
    // The refused publish reaches this node's own subscribers.
    let mut own = a.bus.subscribe(&acme);
    a.bus.publish(&acme, invalidation(5)).await.unwrap();
    assert!(is_change(heard(&mut own).await, 5));
    assert!(a.status.bus_degraded());

    // The lease lapses; node B takes the work. Only B runs it.
    tokio::time::sleep(ttl + Duration::from_millis(500)).await;
    let taken = b
        .coordinator
        .acquire(name, "node-b", Duration::from_secs(30))
        .await
        .unwrap();

    // The server returns to node A. Its old lease is refused: no double run.
    proxy.restore().await;
    wait_until(|| async {
        a.service.probe_once().await;
        a.status.connected()
    })
    .await;
    let stale = a.coordinator.renew(&held, ttl).await.unwrap_err();
    assert_eq!(stale.code, LEASE_LOST);
    assert_eq!(
        a.coordinator
            .acquire(name, "node-a", ttl)
            .await
            .unwrap_err()
            .code,
        LEASE_HELD
    );
    // Nothing cached before the outage is read afterwards (the namespace was reset).
    assert_eq!(a.status.cache_resets(), 1);
    assert_eq!(read(&a.cache, &acme, "warm").await, None);
    // Recovery resumes: when B lets go, A claims with a higher token.
    b.coordinator.release(&taken).await.unwrap();
    let again = a.coordinator.acquire(name, "node-a", ttl).await.unwrap();
    assert!(again.token.0 > taken.token.0);
    a.coordinator.release(&again).await.unwrap();
}

async fn wait_until<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the condition did not hold within ten seconds");
}

impl Proxy {
    fn cut(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    async fn restore(&mut self) {
        let listener = TcpListener::bind(self.addr).await.unwrap();
        self.serve(listener);
    }
}

#[tokio::test]
async fn a_clock_far_from_the_server_stops_the_start() {
    let Some(url) = base() else { return };
    let skewed: Arc<dyn Clock> = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let error = build(
        &enabled(&url, &unique_prefix("skew")),
        &site("acme"),
        skewed,
    )
    .await
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("clock differs"), "{error}");
}

#[tokio::test]
async fn the_start_up_check_reports_the_version_and_the_acl() {
    let Some(url) = base() else { return };
    let host = build(
        &enabled(&url, &unique_prefix("report")),
        &site("acme"),
        system(),
    )
    .await
    .unwrap();
    assert!(host.report.version.is_some());
    // The unrestricted default user is reported, not hidden.
    assert!(matches!(
        host.report.acl,
        AclVerdict::Broad(_) | AclVerdict::Unverified(_)
    ));
    let mut strict = enabled(&url, &unique_prefix("strict"));
    strict.require_restricted_acl = true;
    let refused = build(&strict, &site("acme"), system()).await;
    if let AclVerdict::Broad(_) = host.report.acl {
        let error = refused.err().unwrap().to_string();
        assert!(
            error.contains("more than") || error.contains("does not need"),
            "{error}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_restricted_user_starts_and_a_wrong_password_never_leaks() {
    use std::os::unix::fs::PermissionsExt;
    let Ok(url) = std::env::var("SPOT_TEST_REDIS_ACL_URL") else {
        eprintln!(
            "SKIPPED: SPOT_TEST_REDIS_ACL_URL is not set (needs a server with a restricted user)"
        );
        return;
    };
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let write = |name: &str, text: &str| {
        let path = dir.join(format!("{name}-{}", std::process::id()));
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    };
    let good = write("acl-good", "testpw\n");
    let wrong = write("acl-wrong", "not-the-password-9f2\n");

    let mut settings = enabled(&url, &format!("spot:{}", unique_prefix("acl")));
    settings.username = Some("spot".into());
    settings.password_ref = Some(format!("file:{}", good.display()));
    settings.require_restricted_acl = true;
    let host = build(&settings, &site("acme"), system()).await.unwrap();
    assert_eq!(host.report.acl, AclVerdict::Restricted);

    settings.password_ref = Some(format!("file:{}", wrong.display()));
    let error = build(&settings, &site("acme"), system())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        !error.contains("not-the-password-9f2") && !error.contains("testpw"),
        "{error}"
    );
    let _ = (std::fs::remove_file(good), std::fs::remove_file(wrong));
}
