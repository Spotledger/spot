//! The sandbox reaper (WP-ALM-05.T5): the pass over fakes (no database), and one whole pass over a
//! real host (`SPOT_TEST_PG_URL`; skipped with a printed reason when unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, dead_code)]

mod common;

use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spot_cli::services::sandbox_reaper::{
    Destroyed, Destroyer, Entry, FileRegistry, Listing, Outcome, PgDestroyer, Reaper, Recorded,
    Registry, STUCK_AFTER, STUCK_FOR, Settings, Skipped, span,
};
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, Error as CoordError, FencingToken, InMemoryPresence,
    InProcessCoordinator, Lease, NodeId, RateLimit, Result as CoordResult,
};
use spot_core::{Clock, ManualClock};
use time::OffsetDateTime;
use time::macros::datetime;
use tokio::sync::Notify;

const SITE: &str = "prod";

fn t0() -> OffsetDateTime {
    datetime!(2026-11-13 12:00:00 UTC)
}

fn now(clock: &ManualClock) -> i64 {
    clock.now_utc().unix_timestamp()
}

fn entry(name: &str, source: &str, expires_at: i64) -> Entry {
    Entry {
        name: name.to_owned(),
        source: source.to_owned(),
        database: format!("sandbox_{name}"),
        created_at: 1,
        expires_at,
        state: "active".to_owned(),
        keep: false,
    }
}

// ---- doubles ----

struct FakeRegistry(Mutex<Listing>);
impl FakeRegistry {
    fn new(entries: Vec<Entry>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Listing {
            entries,
            real_databases: BTreeSet::new(),
        })))
    }
}
#[async_trait]
impl Registry for FakeRegistry {
    async fn list(&self) -> Result<Listing, String> {
        Ok(self.0.lock().unwrap().clone())
    }
}

/// Records every destroy call; names in `fail` fail, names in `gone` are already gone.
#[derive(Default)]
struct FakeDestroyer {
    calls: Mutex<Vec<String>>,
    fail: Mutex<HashSet<String>>,
    gone: Mutex<HashSet<String>>,
}
#[async_trait]
impl Destroyer for FakeDestroyer {
    async fn destroy(&self, entry: &Entry) -> Result<Destroyed, &'static str> {
        self.calls.lock().unwrap().push(entry.name.clone());
        if self.fail.lock().unwrap().contains(&entry.name) {
            return Err("drop_failed");
        }
        if self.gone.lock().unwrap().contains(&entry.name) {
            return Ok(Destroyed::AlreadyGone);
        }
        Ok(Destroyed::Gone)
    }
}
impl FakeDestroyer {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

/// A coordinator that wraps the real in-process one and can refuse or lose the lease.
struct Switch {
    inner: Arc<dyn Coordinator>,
    refuse: std::sync::atomic::AtomicBool,
    renews_before_loss: AtomicU32,
    releases: AtomicU32,
}
impl Switch {
    async fn new() -> Arc<Self> {
        let inner: Arc<dyn Coordinator> = InProcessCoordinator::start(
            Arc::new(InMemoryPresence::new()),
            NodeId::new("node-1").unwrap(),
            Arc::new(ManualClock::new(t0())),
        )
        .await
        .unwrap();
        Arc::new(Self {
            inner,
            refuse: false.into(),
            renews_before_loss: AtomicU32::new(u32::MAX),
            releases: AtomicU32::new(0),
        })
    }
}
impl Coordinator for Switch {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, CoordResult<Lease>> {
        if self.refuse.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(CoordError::new(
                    spot_coordinator::error::LEASE_HELD,
                    "another node holds it (test)",
                ))
            });
        }
        self.inner.acquire(name, holder, ttl)
    }
    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, CoordResult<Lease>> {
        if self.renews_before_loss.load(Ordering::SeqCst) == 0 {
            return Box::pin(async {
                Err(CoordError::new(
                    spot_coordinator::error::LEASE_LOST,
                    "the lease was lost (test)",
                ))
            });
        }
        let left = self.renews_before_loss.load(Ordering::SeqCst);
        if left != u32::MAX {
            self.renews_before_loss.store(left - 1, Ordering::SeqCst);
        }
        self.inner.renew(lease, ttl)
    }
    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, CoordResult<()>> {
        self.releases.fetch_add(1, Ordering::SeqCst);
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
}

struct Rig {
    reaper: Reaper,
    registry: Arc<FakeRegistry>,
    destroyer: Arc<FakeDestroyer>,
    clock: Arc<ManualClock>,
}

fn rig(entries: Vec<Entry>, max_per_pass: usize, coordinator: Option<Arc<dyn Coordinator>>) -> Rig {
    let clock = Arc::new(ManualClock::new(t0()));
    let registry = FakeRegistry::new(entries);
    let destroyer = Arc::new(FakeDestroyer::default());
    let settings = Settings {
        enabled: true,
        interval_seconds: 300,
        max_per_pass,
    };
    let reaper = Reaper::new(
        SITE,
        "node-test",
        settings,
        registry.clone(),
        destroyer.clone(),
        coordinator,
        clock.clone(),
    );
    Rig {
        reaper,
        registry,
        destroyer,
        clock,
    }
}

// ---- the pass over fakes ----

#[test]
fn settings_are_checked_and_the_service_is_off_by_default() {
    let defaults = Settings::default();
    assert!(!defaults.enabled, "a host turns the reaper on on purpose");
    assert!(defaults.validate().is_ok());
    for bad in [
        Settings {
            interval_seconds: 29,
            ..Settings::default()
        },
        Settings {
            interval_seconds: 86_401,
            ..Settings::default()
        },
        Settings {
            max_per_pass: 0,
            ..Settings::default()
        },
        Settings {
            max_per_pass: 51,
            ..Settings::default()
        },
    ] {
        assert!(bad.validate().is_err());
    }
    assert!(toml::from_str::<Settings>("surprise = 1").is_err());
    assert_eq!(span(3 * 86_400 + 4 * 3_600), "3d 4h");
    assert_eq!(span(5 * 3_600 + 720), "5h 12m");
    assert_eq!(span(420), "7m");
}

#[tokio::test]
async fn nothing_expired_means_no_destroy_call() {
    let t = now(&ManualClock::new(t0()));
    let rig = rig(vec![entry("a", SITE, t + 1_000)], 5, None);
    let report = rig.reaper.run_pass().await;
    assert_eq!(report.results, vec![]);
    assert!(rig.destroyer.calls().is_empty());
    assert_eq!(
        rig.reaper.status_line(),
        "sandbox_reaper: next expiry in 16m, 0 stuck, no lease (single node)"
    );
}

#[tokio::test]
async fn expired_ones_go_oldest_first_and_max_per_pass_is_respected() {
    let t = now(&ManualClock::new(t0()));
    let rig = rig(
        vec![
            entry("late", SITE, t - 10),
            entry("early", SITE, t - 500),
            entry("middle", SITE, t - 100),
        ],
        2,
        None,
    );
    let first = rig.reaper.run_pass().await;
    assert_eq!(rig.destroyer.calls(), ["early", "middle"]);
    assert_eq!(first.remaining, 1);
    // The remainder goes in the next pass (the fake registry still lists the first two; the real
    // one no longer does, so remove them as destroying would).
    rig.registry
        .0
        .lock()
        .unwrap()
        .entries
        .retain(|e| e.name == "late");
    rig.reaper.run_pass().await;
    assert_eq!(rig.destroyer.calls(), ["early", "middle", "late"]);
}

#[tokio::test]
async fn already_gone_is_success_and_a_failure_does_not_stop_the_pass() {
    let t = now(&ManualClock::new(t0()));
    let rig = rig(
        vec![
            entry("stuck", SITE, t - 300),
            entry("gone", SITE, t - 200),
            entry("fine", SITE, t - 100),
        ],
        5,
        None,
    );
    rig.destroyer.fail.lock().unwrap().insert("stuck".into());
    rig.destroyer.gone.lock().unwrap().insert("gone".into());
    let report = rig.reaper.run_pass().await;
    let outcomes: Vec<(&str, Outcome)> = report
        .results
        .iter()
        .map(|r| (r.id.as_str(), r.outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("stuck", Outcome::Failed("drop_failed")),
            ("gone", Outcome::AlreadyGone),
            ("fine", Outcome::Destroyed),
        ]
    );
    // The record names ids and codes only.
    let text = format!("{:?}", rig.reaper.history());
    assert!(!text.contains("sandbox_"), "{text}");
}

#[tokio::test]
async fn three_failed_passes_in_a_row_skip_the_sandbox_for_an_hour() {
    let t = now(&ManualClock::new(t0()));
    let rig = rig(vec![entry("stuck", SITE, t - 300)], 5, None);
    rig.destroyer.fail.lock().unwrap().insert("stuck".into());
    for _ in 0..STUCK_AFTER {
        rig.reaper.run_pass().await;
    }
    assert_eq!(rig.destroyer.calls().len(), STUCK_AFTER as usize);
    assert!(
        rig.reaper.status_line().contains("1 stuck"),
        "{}",
        rig.reaper.status_line()
    );
    // Skipped while the hour lasts.
    rig.clock.advance(Duration::from_secs(600));
    rig.reaper.run_pass().await;
    assert_eq!(
        rig.destroyer.calls().len(),
        STUCK_AFTER as usize,
        "skipped for an hour"
    );
    // Tried again after it.
    rig.clock
        .advance(Duration::from_secs(STUCK_FOR.unsigned_abs()));
    rig.reaper.run_pass().await;
    assert_eq!(rig.destroyer.calls().len(), STUCK_AFTER as usize + 1);
}

#[tokio::test]
async fn the_safety_rules_leave_these_alone() {
    let t = now(&ManualClock::new(t0()));
    let mut kept = entry("kept", SITE, t - 100);
    kept.keep = true;
    let mut no_expiry = entry("noexpiry", SITE, 0);
    no_expiry.state = "active".to_owned();
    let mut foreign_database = entry("odd", SITE, t - 100);
    foreign_database.database = "production_db".to_owned();
    // `prod` is the source of a registered sandbox, and a row for it is (wrongly) expired.
    let mut the_source = entry("prod", SITE, t - 100);
    the_source.database = "sandbox_prod".to_owned();
    let child = entry("child", "prod", t + 1_000);
    // A sandbox of another site is another site's business.
    let other = entry("theirs", "other", t - 100);
    let rig = rig(
        vec![kept, no_expiry, foreign_database, the_source, child, other],
        5,
        None,
    );
    // A real site's database named by a record.
    rig.registry
        .0
        .lock()
        .unwrap()
        .real_databases
        .insert("sandbox_real".to_owned());
    rig.registry
        .0
        .lock()
        .unwrap()
        .entries
        .push(entry("real", SITE, t - 100));
    let report = rig.reaper.run_pass().await;
    assert_eq!(report.results, vec![]);
    assert!(
        rig.destroyer.calls().is_empty(),
        "{:?}",
        rig.destroyer.calls()
    );
}

#[tokio::test]
async fn a_creation_that_never_finished_is_reaped_after_an_hour() {
    let t = now(&ManualClock::new(t0()));
    let mut creating = entry("half", SITE, 0);
    creating.state = "creating".to_owned();
    creating.created_at = t - 4_000;
    let rig = rig(vec![creating], 5, None);
    rig.reaper.run_pass().await;
    assert_eq!(rig.destroyer.calls(), ["half"]);
}

#[tokio::test]
async fn a_clock_that_went_backwards_skips_the_pass() {
    let t = now(&ManualClock::new(t0()));
    let rig = rig(vec![entry("a", SITE, t - 100)], 5, None);
    rig.reaper.run_pass().await;
    assert_eq!(rig.destroyer.calls().len(), 1);
    // Back by more than the interval (300 s): no reaping on a suspicious clock.
    rig.clock.set_wall(t0() - time::Duration::seconds(3_600));
    let report = rig.reaper.run_pass().await;
    assert_eq!(report.skipped, Some(Skipped::ClockWentBack));
    assert_eq!(rig.destroyer.calls().len(), 1);
}

#[tokio::test]
async fn the_lease_is_required_lost_mid_pass_stops_and_is_released_even_on_error() {
    let t = now(&ManualClock::new(t0()));
    let entries = vec![
        entry("a", SITE, t - 300),
        entry("b", SITE, t - 200),
        entry("c", SITE, t - 100),
    ];

    // Another node holds it: nothing is destroyed.
    let refusing = Switch::new().await;
    refusing.refuse.store(true, Ordering::SeqCst);
    let rig1 = rig(
        entries.clone(),
        5,
        Some(refusing.clone() as Arc<dyn Coordinator>),
    );
    let report = rig1.reaper.run_pass().await;
    assert_eq!(report.skipped, Some(Skipped::LeaseHeld));
    assert!(rig1.destroyer.calls().is_empty());

    // Lost after the first renewal: stops before the next destruction.
    let flaky = Switch::new().await;
    flaky.renews_before_loss.store(1, Ordering::SeqCst);
    let rig2 = rig(
        entries.clone(),
        5,
        Some(flaky.clone() as Arc<dyn Coordinator>),
    );
    let report = rig2.reaper.run_pass().await;
    assert!(report.lease_lost);
    assert_eq!(rig2.destroyer.calls(), ["a"]);
    assert_eq!(
        flaky.releases.load(Ordering::SeqCst),
        1,
        "released at the end"
    );

    // A failing destroy still releases the lease.
    let good = Switch::new().await;
    let rig3 = rig(entries, 5, Some(good.clone() as Arc<dyn Coordinator>));
    rig3.destroyer.fail.lock().unwrap().insert("a".into());
    rig3.reaper.run_pass().await;
    assert_eq!(good.releases.load(Ordering::SeqCst), 1);
    assert_eq!(rig3.destroyer.calls().len(), 3);
}

#[tokio::test]
async fn two_sites_reap_only_their_own_sandboxes() {
    let t = now(&ManualClock::new(t0()));
    let registry = FakeRegistry::new(vec![
        entry("mine", "prod", t - 100),
        entry("theirs", "other", t - 100),
    ]);
    let clock = Arc::new(ManualClock::new(t0()));
    let build = |site: &str, destroyer: &Arc<FakeDestroyer>| {
        Reaper::new(
            site,
            "n",
            Settings {
                enabled: true,
                ..Settings::default()
            },
            registry.clone(),
            destroyer.clone(),
            None,
            clock.clone(),
        )
    };
    let (da, db) = (
        Arc::new(FakeDestroyer::default()),
        Arc::new(FakeDestroyer::default()),
    );
    build("prod", &da).run_pass().await;
    build("other", &db).run_pass().await;
    assert_eq!(da.calls(), ["mine"]);
    assert_eq!(db.calls(), ["theirs"]);
}

// ---- over a real host ----

#[tokio::test(flavor = "multi_thread")]
async fn one_pass_destroys_an_expired_sandbox_and_is_idempotent() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let source = env.site.clone();
    let _secret = env.site();
    let name = format!("{source}-reap");
    let created = common::run(
        &root,
        &[
            "sandbox", "create", "--from", &source, "--name", &name, "--ttl", "1h",
        ],
        None,
    );
    assert!(
        created.status.success(),
        "{}{}",
        String::from_utf8_lossy(&created.stdout),
        String::from_utf8_lossy(&created.stderr)
    );
    let loaded = spot_cli::config::load(&root, false).unwrap();
    let source_db = loaded
        .sites
        .iter()
        .find(|s| s.site == source)
        .unwrap()
        .database
        .clone();
    let dir = root.join("sites").join(&name);
    let record_path = dir.join("sandbox.toml");
    // The sandbox's time to live has passed: the same edit the T3 test makes.
    let text = std::fs::read_to_string(&record_path).unwrap();
    let database = text
        .lines()
        .find_map(|l| l.strip_prefix("database = "))
        .unwrap()
        .trim_matches('"')
        .to_owned();
    let expired: String = text
        .lines()
        .map(|l| {
            if l.starts_with("expires_at") {
                "expires_at = 1".to_owned()
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&record_path, expired).unwrap();

    let real: BTreeSet<String> = loaded
        .sites
        .iter()
        .filter(|s| s.site != name)
        .map(|s| s.database.clone())
        .collect();
    let admin = loaded.admin.clone();
    let reaper = Reaper::new(
        source.clone(),
        "node-test",
        Settings {
            enabled: true,
            ..Settings::default()
        },
        Arc::new(FileRegistry::new(root.clone(), real)),
        Arc::new(PgDestroyer::new(root.clone(), admin.clone())),
        None,
        Arc::new(spot_core::SystemClock::new()),
    );
    let report = reaper.run_pass().await;
    assert_eq!(
        report
            .results
            .iter()
            .map(|r| (r.id.as_str(), r.outcome))
            .collect::<Vec<_>>(),
        [(name.as_str(), Outcome::Destroyed)]
    );
    assert!(!dir.exists(), "the folder and its record are gone");
    // The database is gone, the source is not.
    let backend = spot_postgres::PgBackend::connect(admin.clone().with_dbname("postgres"))
        .await
        .unwrap();
    let client = backend.pool().get().await.unwrap();
    for (db, want) in [(&database, false), (&source_db, true)] {
        let found = client
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[db])
            .await
            .unwrap()
            .is_some();
        assert_eq!(found, want, "{db}");
    }
    assert!(
        root.join("sites").join(&source).exists(),
        "the source is untouched"
    );
    assert!(reaper.status_line().starts_with("sandbox_reaper:"));
    // A second pass finds nothing to do.
    let again = reaper.run_pass().await;
    assert_eq!(again.results, Vec::<Recorded>::new());
}
