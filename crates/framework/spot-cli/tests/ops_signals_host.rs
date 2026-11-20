//! The signals service in a real `spot serve` site (WP-OPS-01.T3). Real PostgreSQL through
//! `SPOT_TEST_PG_URL` (skipped with a printed reason when unset, as the other host tests do).
//!
//! The site hosts the maintenance gate and no journal, sync hub or quarantine: after the gate is
//! closed, one collection later `/metrics` (with the administrator credential) shows
//! `spot_migration_gate_closed 1`, and no `spot_journal_*`, `spot_sync_*` or `spot_quarantine_*`
//! series exists, because no such source is hosted (a zero would read as "healthy"). Without the
//! credential the route is still refused.
//!
//! The open-conditions series is covered by the module's unit tests: nothing a test can reach
//! raises a condition on a started `spot serve` (the handle is a service port, not on `Running`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, dead_code)]

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spot_cli::services::ops_signals::{
    Board, Collector, Kind, MAX_STALE_INTERVALS, QuarantineReading, Reading, Settings, Source,
    SyncReading, Transition, UNKNOWN, aggregate, map_journal, map_quarantine,
};
use spot_cli::services::quarantine::{Item, State};
use spot_core::ManualClock;
use spot_mgmt_journal::Position;
use spot_mgmt_journal::results::{
    AuditOutcome, AuditResult, JournalState, SnapshotInfo, SnapshotListResult, StatusResult,
};
use spot_ops_signals::catalogue::SERIES;
use spot_ops_signals::conditions::OpenCounts;
use time::OffsetDateTime;
use time::macros::datetime;

use common::{ADMIN_PASSWORD, Env, admin, http, start};
use spot_core::{SiteId, SystemClock};
use spot_migration_apply::gate::AdmissionGate;
use spot_migration_apply_pg::PgAdmissionGate;
use spot_postgres::PgBackend;

const SERVICES: &str = "[services.maintenance_gate]\nrefresh_ms = 100\nttl_ms = 100\n\
                        [services.ops_signals]\ninterval_seconds = 5\ntimeout_seconds = 2\n";

async fn pool_of(env: &Env) -> deadpool_postgres::Pool {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    let pg = loaded.admin.clone().with_dbname(&site.database);
    PgBackend::connect(pg).await.unwrap().pool()
}

fn unix_micros() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros(),
    )
    .unwrap()
}

async fn scrape(addr: std::net::SocketAddr, env: &Env, token: Option<&str>) -> (u16, String) {
    let reply = http(addr, &env.site, token, "GET", "/metrics", &[], None).await;
    (
        reply.status,
        String::from_utf8_lossy(&reply.bytes).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_gate_shows_on_metrics_and_unhosted_sources_stay_absent() {
    let Some(env) = Env::new(SERVICES).await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    let first = env.site();
    let started = start(&env).await;
    let token = admin(started.addr, &env.site, &first).await;
    let _ = ADMIN_PASSWORD;

    // The route still demands the credential and the administrator check.
    let (status, text) = scrape(started.addr, &env, None).await;
    assert_ne!(status, 200, "metrics served without a credential");
    assert!(!text.contains("spot_migration_gate_closed"));

    // Close the gate through the same control the migration engine uses (`/metrics` is exempt
    // from the gate), then wait for one collection (the interval is 5 s).
    let gate = PgAdmissionGate::new(
        pool_of(&env).await,
        SiteId::new(&env.site).unwrap(),
        Arc::new(SystemClock::new()),
        Duration::from_millis(100),
    );
    gate.close("signals test", unix_micros()).await.unwrap();

    let mut seen = String::new();
    for _ in 0..40 {
        let (status, text) = scrape(started.addr, &env, Some(&token)).await;
        assert_eq!(status, 200, "{text}");
        if text
            .lines()
            .any(|l| l.starts_with("spot_migration_gate_closed ") && l.ends_with(" 1"))
        {
            seen = text;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(!seen.is_empty(), "the gauge never showed the closed gate");
    for absent in [
        "spot_journal_",
        "spot_sync_nodes",
        "spot_sync_node_lag",
        "spot_quarantine_",
    ] {
        assert!(
            !seen
                .lines()
                .filter(|l| !l.starts_with('#'))
                .any(|l| l.starts_with(absent)),
            "a series of an unhosted source exists: {absent}"
        );
    }

    // Opening the gate shows 0, not an absent series.
    gate.open(unix_micros()).await.unwrap();
    let mut zero = false;
    for _ in 0..40 {
        let (_, text) = scrape(started.addr, &env, Some(&token)).await;
        if text
            .lines()
            .any(|l| l.starts_with("spot_migration_gate_closed ") && l.ends_with(" 0"))
        {
            zero = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(zero, "the gauge never went back to 0");
    started.finish().await;
}

// ---- the module's unit tests (kept here: the module file stays under the line gate) ----

fn now() -> OffsetDateTime {
    datetime!(2026-11-13 12:00:00 UTC)
}

fn status(
    state: JournalState,
    head: Option<(u64, u64)>,
    shipped: Option<(u64, u64)>,
) -> StatusResult {
    let pos = |(generation, seq)| Position { generation, seq };
    StatusResult {
        state,
        head: head.map(pos),
        shipped: shipped.map(pos),
        unshipped_segments: 0,
        policy: spot_mgmt_journal::PolicySetting::Local,
    }
}

fn snapshots(taken_at: &[&str]) -> SnapshotListResult {
    SnapshotListResult {
        snapshots: taken_at
            .iter()
            .map(|t| SnapshotInfo {
                id: "s".to_owned(),
                position: Position {
                    generation: 1,
                    seq: 1,
                },
                taken_at: (*t).to_owned(),
                bytes: 1,
            })
            .collect(),
    }
}

fn audit(outcome: AuditOutcome, ran_at: Option<&str>) -> AuditResult {
    AuditResult {
        outcome,
        ran_at: ran_at.map(str::to_owned),
        records_compared: 0,
        first_difference: None,
        reason: None,
    }
}

#[test]
fn settings_are_checked() {
    assert!(Settings::default().validate().is_ok());
    let low = Settings {
        interval_seconds: 4,
        ..Settings::default()
    };
    assert!(low.validate().is_err());
    let high = Settings {
        interval_seconds: 301,
        ..Settings::default()
    };
    assert!(high.validate().is_err());
    let slow = Settings {
        timeout_seconds: 16,
        ..Settings::default()
    };
    assert!(slow.validate().is_err());
    assert!(toml::from_str::<Settings>("bogus = 1").is_err());
}

#[test]
fn the_journal_maps_to_gaps_and_ages() {
    let st = status(JournalState::Ok, Some((1, 120)), Some((1, 100)));
    let snaps = snapshots(&["2026-11-13T10:00:00Z"]);
    let clean = audit(AuditOutcome::Match, Some("2026-11-12T12:00:00Z"));
    let r = map_journal(&st, &snaps, &clean, now());
    assert_eq!(r.unshipped_entries, Some(20));
    assert_eq!(r.snapshot_age, Some(Duration::from_secs(7200)));
    assert_eq!(r.audit_age, Some(Duration::from_secs(86_400)));

    // A mismatch is not a clean audit: no age. A new generation cannot be compared.
    let bad = audit(AuditOutcome::Mismatch, Some("2026-11-12T12:00:00Z"));
    let st2 = status(JournalState::Degraded, Some((2, 5)), Some((1, 100)));
    let r = map_journal(&st2, &snaps, &bad, now());
    assert_eq!(r.audit_age, None);
    assert_eq!(r.unshipped_entries, None);
    // Nothing shipped yet: the whole head. No target: unknown.
    let st3 = status(JournalState::Ok, Some((1, 7)), None);
    assert_eq!(
        map_journal(&st3, &snaps, &bad, now()).unshipped_entries,
        Some(7)
    );
    let st4 = status(JournalState::NoTarget, Some((1, 7)), None);
    assert_eq!(
        map_journal(&st4, &snaps, &bad, now()).unshipped_entries,
        None
    );
    // No snapshot yet, a never-run audit.
    let none = map_journal(
        &st,
        &snapshots(&[]),
        &audit(AuditOutcome::NeverRun, None),
        now(),
    );
    assert_eq!((none.snapshot_age, none.audit_age), (None, None));
    // A future timestamp clamps to zero.
    let future = snapshots(&["2026-11-14T00:00:00Z"]);
    assert_eq!(
        map_journal(&st, &future, &clean, now()).snapshot_age,
        Some(Duration::ZERO)
    );
    // Garbage in a timestamp is no age, not a panic.
    let junk = snapshots(&["not a time"]);
    assert_eq!(map_journal(&st, &junk, &clean, now()).snapshot_age, None);
}

#[test]
fn aggregation_sums_counts_and_takes_the_oldest_age() {
    let a = Reading::Quarantine(QuarantineReading {
        open: 2,
        oldest_age: Duration::from_secs(10),
    });
    let b = Reading::Quarantine(QuarantineReading {
        open: 3,
        oldest_age: Duration::from_secs(99),
    });
    assert_eq!(
        aggregate(&[a, b]),
        Some(Reading::Quarantine(QuarantineReading {
            open: 5,
            oldest_age: Duration::from_secs(99)
        }))
    );
    let big = Reading::Quarantine(QuarantineReading {
        open: u64::MAX,
        oldest_age: Duration::ZERO,
    });
    let one = Reading::Quarantine(QuarantineReading {
        open: 1,
        oldest_age: Duration::ZERO,
    });
    assert_eq!(
        aggregate(&[big, one]),
        Some(Reading::Quarantine(QuarantineReading {
            open: u64::MAX,
            oldest_age: Duration::ZERO
        }))
    );
    assert_eq!(
        aggregate(&[Reading::Gate(false), Reading::Gate(true)]),
        Some(Reading::Gate(true))
    );
    assert_eq!(aggregate(&[]), None);
    let c = Reading::Conditions(OpenCounts {
        info: 1,
        amber: 0,
        red: 2,
    });
    let d = Reading::Conditions(OpenCounts {
        info: 0,
        amber: 4,
        red: 1,
    });
    assert_eq!(
        aggregate(&[c, d]),
        Some(Reading::Conditions(OpenCounts {
            info: 1,
            amber: 4,
            red: 3
        }))
    );
}

#[test]
fn quarantine_counts_open_items_only() {
    let item = |state, age_secs| Item {
        id: "i".to_owned(),
        doctype: "d".to_owned(),
        reason: "r".to_owned(),
        node: "n".to_owned(),
        record: "x".to_owned(),
        state,
        age_secs,
    };
    let items = vec![
        item(State::Held, 100),
        item(State::Parked, 500),
        item(State::Closed, 9_999),
        item(State::Discarded, 9_999),
        item(State::Retrying, -5),
    ];
    let r = map_quarantine(&items);
    assert_eq!(r.open, 3);
    assert_eq!(r.oldest_age, Duration::from_secs(500));
    assert_eq!(map_quarantine(&[]), QuarantineReading::default());
}

// ---- the collector over fake sources ----

struct Fake {
    kind: Kind,
    reading: Mutex<Option<Reading>>,
    calls: AtomicU32,
    sleep: Duration,
}

impl Fake {
    fn new(kind: Kind, reading: Option<Reading>) -> Arc<Self> {
        Arc::new(Self {
            kind,
            reading: Mutex::new(reading),
            calls: AtomicU32::new(0),
            sleep: Duration::ZERO,
        })
    }
    fn set(&self, reading: Option<Reading>) {
        *self.reading.lock().unwrap() = reading;
    }
}

#[async_trait]
impl Source for Fake {
    fn kind(&self) -> Kind {
        self.kind
    }
    async fn read(&self, _now: OffsetDateTime) -> std::result::Result<Reading, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.sleep.is_zero() {
            tokio::time::sleep(self.sleep).await;
        }
        self.reading
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "down".to_owned())
    }
}

fn collector(
    site: &str,
    board: &Arc<Board>,
    sources: Vec<Arc<dyn Source>>,
    timeout: Duration,
) -> Collector {
    Collector::new(
        site,
        sources,
        Arc::clone(board),
        Arc::new(ManualClock::new(now())),
        timeout,
    )
}

#[tokio::test]
async fn an_absent_source_sets_nothing() {
    let board = Arc::new(Board::new());
    let c = collector("a", &board, vec![], Duration::from_secs(1));
    c.collect_once().await;
    for kind in [
        Kind::Journal,
        Kind::Sync,
        Kind::Quarantine,
        Kind::Conditions,
        Kind::Gate,
    ] {
        assert_eq!(board.aggregate_of(kind), None);
    }
    assert_eq!(c.source_count(), 0);
    assert!(
        c.status_line()
            .starts_with("ops_signals: 0 sources, last ok never")
    );
}

#[tokio::test]
async fn a_failing_source_keeps_its_value_for_three_intervals_then_drops_it() {
    let board = Arc::new(Board::new());
    let gate = Fake::new(Kind::Gate, Some(Reading::Gate(true)));
    let c = collector(
        "a",
        &board,
        vec![gate.clone() as Arc<dyn Source>],
        Duration::from_secs(1),
    );
    c.collect_once().await;
    assert_eq!(board.aggregate_of(Kind::Gate), Some(Reading::Gate(true)));
    gate.set(None);
    for _ in 0..MAX_STALE_INTERVALS {
        c.collect_once().await;
        assert_eq!(
            board.aggregate_of(Kind::Gate),
            Some(Reading::Gate(true)),
            "kept"
        );
    }
    c.collect_once().await;
    assert_eq!(
        board.aggregate_of(Kind::Gate),
        None,
        "dropped on the fourth failing interval"
    );
    // Once per transition: one `Failing`, one `Dropped`, however many ticks failed.
    c.collect_once().await;
    c.collect_once().await;
    assert_eq!(
        c.transitions(),
        vec![
            Transition::Failing(Kind::Gate),
            Transition::Dropped(Kind::Gate)
        ]
    );
    gate.set(Some(Reading::Gate(false)));
    c.collect_once().await;
    c.collect_once().await;
    assert_eq!(board.aggregate_of(Kind::Gate), Some(Reading::Gate(false)));
    assert_eq!(
        c.transitions(),
        vec![
            Transition::Failing(Kind::Gate),
            Transition::Dropped(Kind::Gate),
            Transition::Readable(Kind::Gate)
        ]
    );
}

#[tokio::test]
async fn a_slow_source_does_not_delay_the_others() {
    let board = Arc::new(Board::new());
    let slow = Arc::new(Fake {
        kind: Kind::Sync,
        reading: Mutex::new(Some(Reading::Sync(SyncReading::default()))),
        calls: AtomicU32::new(0),
        sleep: Duration::from_secs(30),
    });
    let fast = Fake::new(Kind::Gate, Some(Reading::Gate(true)));
    let c = collector(
        "a",
        &board,
        vec![slow.clone() as Arc<dyn Source>, fast as Arc<dyn Source>],
        Duration::from_millis(100),
    );
    let started = std::time::Instant::now();
    c.collect_once().await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "waited for the slow source"
    );
    assert_eq!(board.aggregate_of(Kind::Gate), Some(Reading::Gate(true)));
    assert_eq!(
        board.aggregate_of(Kind::Sync),
        None,
        "a timed-out source sets nothing"
    );
    assert_eq!(c.transitions(), vec![Transition::Failing(Kind::Sync)]);
}

#[tokio::test]
async fn two_sites_publish_one_aggregate_and_a_withdrawn_site_leaves_the_other() {
    let board = Arc::new(Board::new());
    let a = Fake::new(
        Kind::Conditions,
        Some(Reading::Conditions(OpenCounts {
            info: 0,
            amber: 1,
            red: 2,
        })),
    );
    let b = Fake::new(
        Kind::Conditions,
        Some(Reading::Conditions(OpenCounts {
            info: 1,
            amber: 0,
            red: 1,
        })),
    );
    let ca = collector(
        "site-a",
        &board,
        vec![a as Arc<dyn Source>],
        Duration::from_secs(1),
    );
    let cb = collector(
        "site-b",
        &board,
        vec![b as Arc<dyn Source>],
        Duration::from_secs(1),
    );
    ca.collect_once().await;
    cb.collect_once().await;
    assert_eq!(
        board.aggregate_of(Kind::Conditions),
        Some(Reading::Conditions(OpenCounts {
            info: 1,
            amber: 1,
            red: 3
        }))
    );
    ca.withdraw();
    assert_eq!(
        board.aggregate_of(Kind::Conditions),
        Some(Reading::Conditions(OpenCounts {
            info: 1,
            amber: 0,
            red: 1
        }))
    );
    cb.withdraw();
    assert_eq!(board.aggregate_of(Kind::Conditions), None);
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn the_sentinel_is_negative_and_covers_every_series_of_the_kind() {
    assert!(UNKNOWN < 0.0);
    for kind in [
        Kind::Journal,
        Kind::Sync,
        Kind::Quarantine,
        Kind::Conditions,
        Kind::Gate,
    ] {
        for name in kind.gauges() {
            assert!(
                SERIES.iter().any(|s| s.name == *name),
                "{name} is not in the catalogue"
            );
        }
    }
}
