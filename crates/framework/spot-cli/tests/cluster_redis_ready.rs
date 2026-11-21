//! `/api/health/ready` shows the cluster-redis degradation live, and the Redis cache never serves a
//! read between a reconnect and the supervisor's cache reset (WP-PLT-19.T4).
//!
//! The readiness tests run without a server (a fake service reports a reason). The cache test
//! needs one: `SPOT_TEST_REDIS_URL` (for example `redis://127.0.0.1:6391`), printing `SKIPPED`
//! with the reason when it is unset. It puts the server behind a TCP proxy it can cut and restore,
//! and sets `probe_interval_ms` to an hour so the supervisor's probe never helps.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_cli::services::cluster_redis::{Settings, build};
use spot_cli::services::{CheckFuture, Level, RunFuture, Service, Services};
use spot_core::{Clock, SiteId, SystemClock};
use spot_http::health::ReadinessCheck;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// A service that can say it is degraded.
struct Fake {
    level: Level,
    reason: Mutex<Option<String>>,
    broken_check: AtomicBool,
}

impl Fake {
    fn new(level: Level) -> Arc<Self> {
        Arc::new(Self {
            level,
            reason: Mutex::new(None),
            broken_check: AtomicBool::new(false),
        })
    }
    fn say(&self, reason: Option<&str>) {
        *self.reason.lock().unwrap() = reason.map(str::to_owned);
    }
}

impl Service for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn level(&self) -> Level {
        self.level
    }
    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if self.broken_check.load(Ordering::SeqCst) {
                Err("not started".to_owned())
            } else {
                Ok(())
            }
        })
    }
    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
    fn degraded(&self) -> Option<String> {
        self.reason.lock().unwrap().clone()
    }
}

async fn assembly(service: Arc<Fake>) -> Services {
    let mut services = Services::new("acme", Duration::from_secs(1));
    services.add(service);
    services.start().await.unwrap();
    services
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_service_that_reports_a_reason_degrades_readiness_and_never_fails_it() {
    for level in [Level::Optional, Level::Required] {
        let fake = Fake::new(level);
        let services = assembly(Arc::clone(&fake)).await;
        let check = services.health_check();
        assert!(check.check().await.detail.is_empty(), "healthy: no detail");
        fake.say(Some("the coordination server is unreachable"));
        let degraded = check.check().await;
        assert!(degraded.ok, "degraded, never failed: {}", degraded.detail);
        assert!(
            degraded
                .detail
                .starts_with("degraded: fake (the coordination server is unreachable)"),
            "{}",
            degraded.detail
        );
        fake.say(None);
        let recovered = check.check().await;
        assert!(
            recovered.ok && recovered.detail.is_empty(),
            "{}",
            recovered.detail
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_default_service_reports_nothing_and_a_stopped_one_is_not_asked() {
    let fake = Fake::new(Level::Optional);
    fake.broken_check.store(true, Ordering::SeqCst);
    fake.say(Some("would be shown if it ran"));
    let mut services = Services::new("acme", Duration::from_secs(1));
    services.add(Arc::clone(&fake) as Arc<dyn Service>);
    services.start().await.unwrap();
    let detail = services.health_check().check().await.detail;
    // Stopped: the start-up reason shows, the live reason of a service that never ran does not.
    assert!(detail.contains("not started"), "{detail}");
    assert!(!detail.contains("would be shown"), "{detail}");
}

// ---- the cache after an outage (needs a server) -----------------------------------------

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_very_next_cache_read_after_recovery_is_a_miss_even_with_an_hour_between_probes() {
    let Ok(url) = std::env::var("SPOT_TEST_REDIS_URL") else {
        eprintln!("SKIPPED: SPOT_TEST_REDIS_URL is not set (needs a Redis or Valkey server)");
        return;
    };
    let target = url.trim_start_matches("redis://").to_owned();
    let mut proxy = Proxy::start(&target).await;
    let settings = Settings {
        enabled: true,
        url: Some(format!("redis://{}", proxy.addr)),
        allow_plaintext: true,
        prefix: format!("t4:{}", std::process::id()),
        probe_interval_ms: 3_600_000,
        ..Settings::default()
    };
    let site = SiteId::new("acme").unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let parts = build(&settings, &site, clock).await.unwrap();
    parts
        .cache
        .set(&site, "k", Arc::from(&b"before the outage"[..]), None)
        .await
        .unwrap();
    assert!(parts.cache.get(&site, "k").await.unwrap().is_some());

    proxy.cut();
    // The supervisor sees the outage (its probe is driven by hand: the interval is an hour).
    parts.service.probe_once().await;
    assert!(
        parts.status.degraded_reason().is_some(),
        "readiness shows the outage"
    );
    assert!(parts.service.degraded().is_some());

    proxy.restore().await;
    // No probe runs: the very next read after recovery must not see the pre-outage entry.
    let mut seen = None;
    for _ in 0..40 {
        seen = parts.cache.get(&site, "k").await.unwrap();
        if parts.status.connected() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        parts.status.connected(),
        "the gate noticed the server is back"
    );
    assert!(
        seen.is_none(),
        "a pre-outage entry was served after recovery"
    );
    assert!(parts.cache.get(&site, "k").await.unwrap().is_none());
    assert_eq!(parts.status.cache_resets(), 1, "reset once, not twice");
    assert!(parts.service.degraded().is_none());
}
