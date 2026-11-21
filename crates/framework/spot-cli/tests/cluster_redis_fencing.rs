//! Two hosts, one lease store: fencing when a host is cut off from Redis, and what an outage
//! shorter than one probe does to the cache (WP-PLT-19.T7).
//!
//! WP-PLT-19.T3 hosts the plugin once per site and WP-PLT-19.T4 shows its degradation; neither
//! cut a host off while a second one ran. These tests do:
//!
//! 1. **Fencing, no server needed.** Two hosts share one coordinator (the in-process one, on a
//!    manual clock, behind the plugin's own `ObservedCoordinator` decorator and a switch that
//!    cuts a host off). Host A holds the lease and runs a job; A is cut off; B takes the lease
//!    once A's expires; A heals and finds it is no longer the holder, does not act, and rejoins
//!    only after B lets go. The job runs once per fencing token, tokens only grow, and A never
//!    acts between its cut and its rejoin.
//! 2. **The same scenario on a real server.** With `SPOT_TEST_REDIS_URL` set, each host connects
//!    through its own TCP proxy that the test cuts and restores (so cutting A leaves B on the
//!    server). Lease times are under a second; no sleep is over a second. Prints `SKIPPED` with
//!    the reason when the variable is unset.
//! 3. **An outage shorter than a probe (needs a server).** `probe_interval_ms` is injected as an
//!    hour, so only a lease call or a manual probe could see an outage. The Redis cache swallows
//!    connection failures (a lost `delete` returns `false`, with no error code), so a cut that
//!    ends before anything looked is not seen: a cache entry that should have been invalidated is
//!    served after the cut. The test pins that behaviour as it is today, so a later change to it
//!    shows up as a failure here and is a deliberate decision, not an accident.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_cli::services::Service;
use spot_cli::services::cluster_redis::{ObservedCoordinator, Settings, Status, build};
use spot_cluster_redis::coordinator::UNAVAILABLE;
use spot_coordinator::error::{LEASE_HELD, LEASE_LOST};
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, FencingToken, InMemoryPresence, InProcessCoordinator, Lease,
    NodeId, RateLimit, Result as CoordResult,
};
use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use time::OffsetDateTime;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const LEASE: &str = "job:acme";

// ---- a coordinator a test can cut off ---------------------------------------------------------

/// Delegates to a shared coordinator, except that while `down` it answers the three calls the
/// plugin watches (`acquire`, `renew`, `check_fence`) with the plugin's `UNAVAILABLE` code, as a
/// host that lost its connection would.
struct Cuttable {
    inner: Arc<dyn Coordinator>,
    down: Arc<AtomicBool>,
}

fn unavailable<T>() -> CoordResult<T> {
    Err(spot_coordinator::Error::new(
        UNAVAILABLE,
        "the coordination server is unreachable",
    ))
}

impl Coordinator for Cuttable {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, CoordResult<Lease>> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { unavailable() });
        }
        self.inner.acquire(name, holder, ttl)
    }
    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, CoordResult<Lease>> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { unavailable() });
        }
        self.inner.renew(lease, ttl)
    }
    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, CoordResult<()>> {
        self.inner.release(lease)
    }
    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<Option<Lease>>> {
        self.inner.current(name)
    }
    fn check_fence<'a>(
        &'a self,
        name: &'a str,
        token: FencingToken,
    ) -> BoxFuture<'a, CoordResult<()>> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { unavailable() });
        }
        self.inner.check_fence(name, token)
    }
    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.next(name, by)
    }
    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.peek(name)
    }
    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.set_at_least(name, value)
    }
    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, CoordResult<Admit>> {
        self.inner.take(key, limit, cost)
    }
    fn waker(&self) -> Arc<Notify> {
        self.inner.waker()
    }
    fn release_now(&self, lease: &Lease) -> bool {
        self.inner.release_now(lease)
    }
}

// ---- a host that runs one job under the lease -------------------------------------------------

/// What one tick of a host did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tick {
    /// Held the lease with a valid fence and ran the job under this token.
    Acted(u64),
    /// Did not hold the lease (someone else does) and did nothing.
    Idle,
    /// Could not reach the store: did nothing (fail closed) and kept its old handle.
    Unavailable,
    /// Learned its lease is gone (expired or superseded): dropped it and did nothing.
    Lost,
}

/// The job's log: who ran it, under which fencing token, in order.
#[derive(Default)]
struct JobLog(Mutex<Vec<(String, u64)>>);

impl JobLog {
    fn runs(&self) -> Vec<(String, u64)> {
        self.0.lock().unwrap().clone()
    }
}

struct Host {
    name: &'static str,
    coordinator: Arc<dyn Coordinator>,
    status: Arc<Status>,
    lease: Mutex<Option<Lease>>,
}

impl Host {
    fn new(name: &'static str, coordinator: Arc<dyn Coordinator>, status: Arc<Status>) -> Self {
        Self {
            name,
            coordinator,
            status,
            lease: Mutex::new(None),
        }
    }

    fn handle(&self) -> Option<Lease> {
        self.lease.lock().unwrap().clone()
    }

    /// Keep or take the lease, check the fence, run the job. The shape of what the scheduler
    /// and the job worker do with a coordinator; nothing is cached across ticks except the
    /// handle, which `renew` refuses once it is stale.
    async fn tick(&self, ttl: Duration, log: &JobLog) -> Tick {
        let lease = match self.handle() {
            Some(held) => match self.coordinator.renew(&held, ttl).await {
                Ok(renewed) => renewed,
                Err(e) if e.code == UNAVAILABLE => return Tick::Unavailable,
                Err(_) => {
                    *self.lease.lock().unwrap() = None;
                    return Tick::Lost;
                }
            },
            None => match self.coordinator.acquire(LEASE, self.name, ttl).await {
                Ok(acquired) => acquired,
                Err(e) if e.code == UNAVAILABLE => return Tick::Unavailable,
                Err(_) => return Tick::Idle,
            },
        };
        *self.lease.lock().unwrap() = Some(lease.clone());
        match self.coordinator.check_fence(LEASE, lease.token).await {
            Ok(()) => {
                log.0
                    .lock()
                    .unwrap()
                    .push((self.name.to_owned(), lease.token.0));
                Tick::Acted(lease.token.0)
            }
            Err(e) if e.code == UNAVAILABLE => Tick::Unavailable,
            Err(_) => {
                *self.lease.lock().unwrap() = None;
                Tick::Lost
            }
        }
    }

    /// Ticks until the store answers (the reconnect after a heal may take a few tries).
    async fn tick_when_reachable(&self, env: &impl World, ttl: Duration, log: &JobLog) -> Tick {
        for _ in 0..40 {
            let tick = self.tick(ttl, log).await;
            if tick != Tick::Unavailable {
                return tick;
            }
            env.pause().await;
        }
        Tick::Unavailable
    }
}

// ---- the world: how the cut, the heal and the passing of a lease differ per variant ----------

#[allow(async_fn_in_trait)]
trait World {
    /// Cuts host A off from the lease store.
    fn cut_a(&mut self);
    /// Lets host A reach it again.
    async fn heal_a(&mut self);
    /// Lets one lease ttl pass.
    async fn pass_ttl(&self, ttl: Duration);
    /// A short wait between tries (nothing in the manual-clock world).
    async fn pause(&self);
}

/// The scenario, once for both worlds. `a` starts as the holder.
async fn scenario(world: &mut impl World, a: &Host, b: &Host, ttl: Duration) {
    let log = JobLog::default();

    // A is the holder and runs the job; B is a hot spare.
    let t1 = match a.tick(ttl, &log).await {
        Tick::Acted(token) => token,
        other => panic!("host A should have acted, got {other:?}"),
    };
    assert_eq!(b.tick(ttl, &log).await, Tick::Idle, "B must not double-run");
    assert_eq!(
        a.tick(ttl, &log).await,
        Tick::Acted(t1),
        "renewal keeps the token"
    );
    assert!(a.status.connected() && b.status.connected());
    let runs_before_cut = log.runs().len();

    // A is cut off. B checks first (A's lease is still live, so B stays idle), then A: it cannot
    // renew, so it does nothing, and its status says why.
    world.cut_a();
    assert_eq!(
        b.tick(ttl, &log).await,
        Tick::Idle,
        "A's lease is still live"
    );
    assert_eq!(a.tick(ttl, &log).await, Tick::Unavailable);
    assert!(
        !a.status.connected(),
        "a lost renewal shows in the status at once"
    );
    assert!(a.status.degraded_reason().is_some());
    assert!(b.status.connected(), "B was never cut off");
    assert_eq!(
        log.runs().len(),
        runs_before_cut,
        "no run while A is unreachable"
    );

    // A's lease runs out; B takes it under a larger token and runs the job. A, still cut off, does
    // nothing at all.
    world.pass_ttl(ttl).await;
    let t2 = match b.tick(ttl, &log).await {
        Tick::Acted(token) => token,
        other => panic!("host B should have taken the lease, got {other:?}"),
    };
    assert!(t2 > t1, "a new holder always gets a larger fencing token");
    assert_eq!(a.tick(ttl, &log).await, Tick::Unavailable);
    assert_eq!(
        log.runs(),
        [
            ("host-a".to_owned(), t1),
            ("host-a".to_owned(), t1),
            ("host-b".to_owned(), t2),
        ],
        "A ran twice before the cut (renewal), B once; nothing in between"
    );

    // A heals. The store refuses its old token (a stale writer), refuses its old handle, and A
    // drops the lease without acting. It cannot take the lease while B holds it.
    world.heal_a().await;
    // B renews first, so its lease is fresh however long A's reconnect takes.
    assert_eq!(b.tick(ttl, &log).await, Tick::Acted(t2));
    let Some(stale) = a.handle() else {
        panic!("A should still hold its old handle");
    };
    let fenced = loop {
        match a.coordinator.check_fence(LEASE, stale.token).await {
            Err(e) if e.code == UNAVAILABLE => world.pause().await,
            other => break other,
        }
    };
    assert_eq!(
        fenced.unwrap_err().code,
        LEASE_LOST,
        "a stale token is fenced off"
    );
    assert_eq!(a.tick_when_reachable(&*world, ttl, &log).await, Tick::Lost);
    assert!(
        a.status.connected(),
        "any answer from the server proves it is back"
    );
    assert!(a.handle().is_none());
    assert_eq!(b.tick(ttl, &log).await, Tick::Acted(t2));
    assert_eq!(a.tick(ttl, &log).await, Tick::Idle, "B holds it; A waits");
    assert_eq!(
        a.coordinator
            .acquire(LEASE, "host-a", ttl)
            .await
            .unwrap_err()
            .code,
        LEASE_HELD
    );

    // B lets go (a graceful stop); A rejoins under a still larger token and runs the job.
    let b_lease = b.handle().expect("B holds the lease");
    b.coordinator.release(&b_lease).await.unwrap();
    *b.lease.lock().unwrap() = None;
    let t3 = match a.tick(ttl, &log).await {
        Tick::Acted(token) => token,
        other => panic!("host A should have rejoined, got {other:?}"),
    };
    assert!(t3 > t2);

    // Over the whole run: one holder per token, tokens never go back, and A was silent from its
    // cut until its rejoin.
    let runs = log.runs();
    let tokens: Vec<u64> = runs.iter().map(|(_, token)| *token).collect();
    assert!(tokens.windows(2).all(|w| w[0] <= w[1]), "{runs:?}");
    for token in [t1, t2, t3] {
        let holders: std::collections::BTreeSet<&str> = runs
            .iter()
            .filter(|(_, t)| *t == token)
            .map(|(who, _)| who.as_str())
            .collect();
        assert_eq!(
            holders.len(),
            1,
            "two hosts ran under token {token}: {runs:?}"
        );
    }
    let a_after_cut = runs
        .iter()
        .skip(runs_before_cut)
        .filter(|(who, _)| who == "host-a")
        .count();
    assert_eq!(a_after_cut, 1, "only A's rejoin after the cut: {runs:?}");
    assert_eq!(runs.last().unwrap(), &("host-a".to_owned(), t3));
}

// ---- world 1: the manual clock, no server -----------------------------------------------------

struct Manual {
    clock: Arc<ManualClock>,
    down_a: Arc<AtomicBool>,
}

impl World for Manual {
    fn cut_a(&mut self) {
        self.down_a.store(true, Ordering::SeqCst);
    }
    async fn heal_a(&mut self) {
        self.down_a.store(false, Ordering::SeqCst);
    }
    async fn pass_ttl(&self, ttl: Duration) {
        self.clock.advance(ttl + Duration::from_millis(1));
    }
    async fn pause(&self) {}
}

fn observed(
    inner: Arc<dyn Coordinator>,
    down: Arc<AtomicBool>,
    clock: Arc<ManualClock>,
) -> (Arc<dyn Coordinator>, Arc<Status>) {
    let status = Status::new();
    let cuttable: Arc<dyn Coordinator> = Arc::new(Cuttable { inner, down });
    let coordinator = Arc::new(ObservedCoordinator::new(
        cuttable,
        Arc::clone(&status),
        clock as Arc<dyn Clock>,
    ));
    (coordinator, status)
}

#[tokio::test]
async fn a_host_cut_off_stops_acting_and_the_other_takes_the_job_exactly_once() {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let shared: Arc<dyn Coordinator> = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("store").unwrap(),
        Arc::clone(&clock) as Arc<dyn Clock>,
    )
    .await
    .unwrap();
    let down_a = Arc::new(AtomicBool::new(false));
    let down_b = Arc::new(AtomicBool::new(false));
    let (ca, status_a) = observed(Arc::clone(&shared), Arc::clone(&down_a), Arc::clone(&clock));
    let (cb, status_b) = observed(shared, down_b, Arc::clone(&clock));
    let a = Host::new("host-a", ca, status_a);
    let b = Host::new("host-b", cb, status_b);
    let mut world = Manual { clock, down_a };
    scenario(&mut world, &a, &b, Duration::from_secs(10)).await;
}

// ---- world 2: a real server behind one cuttable proxy per host --------------------------------

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

    /// Stops listening and drops every open connection.
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

struct Live {
    proxy_a: Proxy,
}

impl World for Live {
    fn cut_a(&mut self) {
        self.proxy_a.cut();
    }
    async fn heal_a(&mut self) {
        self.proxy_a.restore().await;
    }
    async fn pass_ttl(&self, ttl: Duration) {
        // No sleep is over a second: wait in slices until the ttl and a margin have passed.
        let mut left = ttl + Duration::from_millis(200);
        while !left.is_zero() {
            let slice = left.min(Duration::from_millis(400));
            tokio::time::sleep(slice).await;
            left -= slice;
        }
    }
    async fn pause(&self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn redis_url() -> Option<String> {
    let url = std::env::var("SPOT_TEST_REDIS_URL").ok();
    if url.is_none() {
        eprintln!("SKIPPED: SPOT_TEST_REDIS_URL is not set (needs a Redis or Valkey server)");
    }
    url
}

fn unique_prefix(tag: &str) -> String {
    format!(
        "t7:{}:{}:{tag}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

fn through(proxy: &Proxy, prefix: &str, probe_ms: u64) -> Settings {
    Settings {
        enabled: true,
        url: Some(format!("redis://{}", proxy.addr)),
        allow_plaintext: true,
        prefix: prefix.to_owned(),
        probe_interval_ms: probe_ms,
        ..Settings::default()
    }
}

#[tokio::test]
async fn on_a_real_server_a_cut_host_is_fenced_off_and_the_job_runs_once_per_token() {
    let Some(url) = redis_url() else { return };
    let target = url.trim_start_matches("redis://").to_owned();
    let proxy_a = Proxy::start(&target).await;
    let proxy_b = Proxy::start(&target).await;
    let prefix = unique_prefix("fence");
    let site = SiteId::new("acme").unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    // One keyspace for both hosts (the same site of the same cluster), one connection each.
    let parts_a = build(
        &through(&proxy_a, &prefix, 3_600_000),
        &site,
        Arc::clone(&clock),
    )
    .await
    .unwrap();
    let parts_b = build(&through(&proxy_b, &prefix, 3_600_000), &site, clock)
        .await
        .unwrap();
    let a = Host::new("host-a", parts_a.coordinator, parts_a.status);
    let b = Host::new("host-b", parts_b.coordinator, parts_b.status);
    let mut world = Live { proxy_a };
    scenario(&mut world, &a, &b, Duration::from_millis(800)).await;
}

// ---- an outage shorter than one probe ---------------------------------------------------------

#[tokio::test]
async fn an_outage_shorter_than_a_probe_is_not_seen_and_a_lost_invalidation_is_served_after_it() {
    let Some(url) = redis_url() else { return };
    let target = url.trim_start_matches("redis://").to_owned();
    let mut proxy = Proxy::start(&target).await;
    // The probe interval is an hour: nothing but a lease call or a probe run by hand could see the
    // outage, and this test makes none.
    let settings = through(&proxy, &unique_prefix("subprobe"), 3_600_000);
    let site = SiteId::new("acme").unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let parts = build(&settings, &site, clock).await.unwrap();
    parts
        .cache
        .set(&site, "k", Arc::from(&b"before the cut"[..]), None)
        .await
        .unwrap();
    assert!(parts.cache.get(&site, "k").await.unwrap().is_some());

    proxy.cut();
    // The invalidation that should have removed the entry cannot reach the server. The cache
    // swallows the failure: `false`, no error, and no flag anywhere.
    let removed = parts.cache.delete(&site, "k").await;
    assert!(!removed, "the delete never reached the server");
    assert!(
        parts.status.connected(),
        "an outage nothing looked at is not seen (no lease call, no probe)"
    );
    proxy.restore().await;

    // After the cut the entry that should be gone is served: today's behaviour, pinned. The
    // gate resets the cache only when the status saw a disconnect, and the status did not.
    let mut seen = None;
    for _ in 0..40 {
        if let Ok(Some(value)) = parts.cache.get(&site, "k").await {
            seen = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        seen.as_deref(),
        Some(&b"before the cut"[..]),
        "TODAY an unseen outage leaves a stale entry readable; if this now fails, the plugin \
         handles sub-probe outages and WP-PLT-19's guide must say so"
    );
    assert_eq!(parts.status.cache_resets(), 0);
    // A probe run after the heal sees a healthy server and resets nothing either.
    parts.service.probe_once().await;
    assert_eq!(parts.status.cache_resets(), 0);
    assert!(parts.service.degraded().is_none());
}
