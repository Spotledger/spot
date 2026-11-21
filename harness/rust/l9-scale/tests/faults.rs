//! Faults under load (WP-TST-15.T7): while a tier M shaped load runs against a real `spot serve`
//! on PostgreSQL, one fault is injected, held and healed, and the run is judged on what a
//! client could see: readiness transitions, the answers during the fault, recovery, and the
//! ledger of acknowledged writes.
//!
//! Needs `SPOT_TEST_PG_URL`; without it each test prints why and returns. A fault that needs
//! privilege or a prepared runner is skipped with a printed reason (`src/faults.rs` lists the
//! environment variables that switch each on); the PostgreSQL connection kill and the pool
//! exhaustion run anywhere. `L9_LOAD_SCALE` scales the background load (default 100 thousandths).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../src/faults.rs"]
mod faults;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use faults::{Fault, Injected, Undo, WriterSession};
use l9_scale::budget::Budgets;
use l9_scale::clock::{Clock, MonotonicClock};
use l9_scale::http::Conn;
use l9_scale::launcher::{self, LaunchOptions, Launched};
use l9_scale::profile::{Decimal, Profile, parse_decimal};
use l9_scale::runner::Sampler;
use l9_scale::{Mode, RunOptions, execute};
use serde_json::json;
use spot_postgres::testing::TestDb;

const SCALE_ENV: &str = "L9_LOAD_SCALE";
const DEFAULT_PERMILLE: u64 = 100;
/// Seconds of load before the fault, and the most seconds recovery may take afterwards.
const WARM_S: u64 = 8;
const HOLD_S: u64 = 8;
const RECOVERY_S: u64 = 60;
/// The consecutive acknowledged attempts that count as recovered.
const RECOVERED_STREAK: usize = 10;
/// Readiness must leave 200 within this long of the fault (the pool's own health check).
const READINESS_DOWN_WITHIN_MS: u64 = 15_000;

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn load_permille() -> u64 {
    std::env::var(SCALE_ENV)
        .ok()
        .and_then(|v| parse_decimal(&v, 3))
        .map_or(DEFAULT_PERMILLE, |n| n.clamp(1, 1000))
}

fn load_profile() -> Profile {
    Profile::load(&crate_dir().join("profiles/faults.toml")).unwrap()
}

/// The load of the run: the profile with sessions and arrival rate scaled by `permille`, and
/// a steady phase long enough for the warm-up, the fault and the recovery window.
fn scaled(profile: &Profile, permille: u64, steady_seconds: u64) -> Profile {
    let mut p = profile.clone();
    p.sessions = u32::try_from((u64::from(p.sessions) * permille / 1000).max(2)).unwrap_or(2);
    let rate = (p.target_milli_tps() / 1000 * permille / 1000).max(1);
    p.target_tps = Decimal::Int(rate);
    p.steady_seconds = steady_seconds;
    p.ramp_seconds = 3;
    p.name = "faults".to_owned();
    p
}

/// Why `fault` cannot run on this machine, before anything is started.
fn preflight(fault: Fault) -> Option<String> {
    use faults::env;
    let missing = |names: &[&str]| {
        let unset: Vec<&&str> = names
            .iter()
            .filter(|n| std::env::var(n).map_or(true, |v| v.trim().is_empty()))
            .collect();
        if unset.is_empty() {
            None
        } else {
            Some(format!("{unset:?} not set"))
        }
    };
    match fault {
        Fault::PgConnectionKill => None,
        Fault::PgRestart => missing(&[env::PG_DATA]),
        Fault::DiskFull => missing(&[env::FULL_DIR]),
        Fault::SlowDisk => missing(&[env::IO_MAX_FILE, env::IO_MAX_SLOW, env::IO_MAX_FAST]),
        Fault::ClockSkew => Some("the host has no --clock-offset hook".to_owned()),
    }
}

async fn scenario(fault: Fault) {
    if let Some(why) = preflight(fault) {
        println!("SKIPPED: {}: {why}", fault.name());
        return;
    }
    let permille = load_permille();
    let profile = scaled(&load_profile(), permille, WARM_S + HOLD_S + RECOVERY_S + 10);
    let sessions = profile.sessions;
    let Some(launched) = Launched::start(&LaunchOptions {
        repo: launcher::default_repo(),
        spot_bin: None,
        users: sessions.max(3),
        run_id: launcher::random_hex(12),
        keep: false,
    })
    .await
    .unwrap_or_else(|e| panic!("cannot start the deployment: {e}")) else {
        println!("SKIPPED: {}: needs SPOT_TEST_PG_URL", fault.name());
        return;
    };
    let db = TestDb::create().await.expect("PostgreSQL is configured");
    let database = faults::database_of(&launched.target.host);
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    let stop = Arc::new(AtomicBool::new(false));
    let run = launcher::random_hex(8);

    // Beside the load: two ledger writers (about 5 writes per second together) and the readiness probe.
    let writers: Vec<_> = launched
        .users
        .iter()
        .take(2)
        .enumerate()
        .map(|(i, u)| {
            tokio::spawn(faults::ledger_writer(
                WriterSession {
                    target: launched.target.clone(),
                    email: u.email.clone(),
                    password: u.password.expose().to_owned(),
                },
                run.clone(),
                (i as u64) * 1_000_000,
                3,
                3,
                stop.clone(),
                clock.clone(),
            ))
        })
        .collect();
    let series: Arc<Mutex<Vec<(u64, u16)>>> = Arc::new(Mutex::new(Vec::new()));
    let probe = tokio::spawn(faults::readiness_probe(
        launched.target.clone(),
        stop.clone(),
        clock.clone(),
        series.clone(),
    ));
    let load = {
        let target = launched.target.clone();
        let users = launched.users.clone();
        let profile = profile.clone();
        tokio::spawn(async move {
            // The load is not budgeted here: errors during a fault are expected, and what a
            // client sees of them is judged through the ledger and the probe.
            let budgets = Budgets::from_toml(
                "[[budget]]\nprofile = \"faults\"\noperation = \"all\"\nmax_error_rate = 1\n",
            )
            .unwrap();
            execute(RunOptions {
                profile,
                budgets,
                mode: Mode::Attach { target, users },
                run_id: launcher::random_hex(12),
                out_dir: None,
            })
            .await
        })
    };

    tokio::time::sleep(Duration::from_secs(WARM_S)).await;
    let injected_ms = clock.now_ns() / 1_000_000;
    let mut undo = Undo::default();
    let injected = faults::inject(fault, &db, &database, &mut undo).await;
    if let Injected::Skipped(why) = &injected {
        println!("SKIPPED: {}: {why}", fault.name());
    } else {
        tokio::time::sleep(Duration::from_secs(HOLD_S)).await;
    }
    faults::heal(&mut undo);
    let healed_ms = clock.now_ns() / 1_000_000;
    tokio::time::sleep(Duration::from_secs(if injected == Injected::Active {
        RECOVERY_S
    } else {
        3
    }))
    .await;
    stop.store(true, Ordering::SeqCst);

    let mut ledgers = Vec::new();
    for w in writers {
        ledgers.push(w.await.unwrap());
    }
    let ledger = faults::merge(ledgers);
    probe.await.unwrap();
    let _ = load.await;
    let series = series.lock().unwrap().clone();

    // What a client could read back, once the fault is over.
    let user = launched.users.first().unwrap().clone();
    let mut conn = Conn::new();
    let body = json!({ "user": user.email, "password": user.password.expose() }).to_string();
    let login = conn
        .request(
            &launched.target,
            "POST",
            "/api/v1/auth/login",
            &[("Content-Type", "application/json")],
            Some(body.as_bytes()),
            faults::REQUEST_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(
        login.status,
        200,
        "sign-in after {} answered {}",
        fault.name(),
        login.status
    );
    let bearer = format!(
        "Bearer {}",
        login
            .json()
            .pointer("/data/token")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
    );
    let codes = faults::all_codes(&launched.target, &bearer).await.unwrap();

    println!(
        "l9 faults: {} injected at {injected_ms} ms, healed at {healed_ms} ms; {} acked, {} unacked, slowest attempt {} ms",
        fault.name(),
        ledger.acked.len(),
        ledger.unacked.len(),
        ledger.slowest_ms
    );
    if injected == Injected::Active {
        // Never a hang beyond the request timeout.
        assert!(
            ledger.slowest_ms <= faults::REQUEST_TIMEOUT.as_millis() as u64 + 1_000,
            "an attempt took {} ms during {}",
            ledger.slowest_ms,
            fault.name()
        );
        // Readiness: it leaves 200 where the fault demands it, and is back.
        let (down, up) = faults::transitions(&series, injected_ms);
        println!("l9 faults: readiness down at {down:?}, up again at {up:?}");
        if fault.expects_not_ready() {
            let down =
                down.unwrap_or_else(|| panic!("readiness never left 200 during {}", fault.name()));
            assert!(
                down - injected_ms <= READINESS_DOWN_WITHIN_MS,
                "readiness took {} ms to leave 200",
                down - injected_ms
            );
            assert!(
                up.is_some(),
                "readiness never returned after {}",
                fault.name()
            );
        }
        assert_eq!(
            series.last().map(|(_, s)| *s),
            Some(200),
            "the host is not ready at the end of {}",
            fault.name()
        );
        // Recovery: the writers are acknowledged again within 60 s of the end of the fault.
        let recovered = ledger.recovered_at(healed_ms, RECOVERED_STREAK);
        assert!(
            recovered.is_some_and(|t| t <= healed_ms + RECOVERY_S * 1000),
            "the error rate did not return to baseline within {RECOVERY_S} s of healing {}",
            fault.name()
        );
    }
    // Always: nothing acknowledged was lost, nothing exists twice, no hidden 500.
    faults::check_ledger(fault, &ledger, &codes);
    assert!(!ledger.acked.is_empty(), "no write was acknowledged at all");
    launched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn postgres_connections_killed_under_load_are_absorbed_without_losing_a_write() {
    scenario(Fault::PgConnectionKill).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn postgres_restarted_under_load_turns_readiness_and_loses_no_acknowledged_write() {
    scenario(Fault::PgRestart).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_disk_filled_to_95_percent_under_load_loses_no_acknowledged_write() {
    scenario(Fault::DiskFull).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_slow_disk_under_load_loses_no_acknowledged_write() {
    scenario(Fault::SlowDisk).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_clock_one_minute_ahead_under_load_loses_no_acknowledged_write() {
    scenario(Fault::ClockSkew).await;
}

/// 400 sessions against a connection pool of 16 (the launched host's default): queueing and
/// bounded latency, not failures; the connections go back to the baseline afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn pool_exhaustion_queues_and_the_connections_return_to_the_baseline() {
    const SESSIONS: usize = 400;
    const REQUESTS: usize = 5;
    let Some(launched) = Launched::start(&LaunchOptions {
        repo: launcher::default_repo(),
        spot_bin: None,
        users: 4,
        run_id: launcher::random_hex(12),
        keep: false,
    })
    .await
    .unwrap_or_else(|e| panic!("cannot start the deployment: {e}")) else {
        println!("SKIPPED: pool exhaustion needs SPOT_TEST_PG_URL");
        return;
    };
    let sampler: Arc<dyn Sampler> = launched.sampler();
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    let mut bearers = Vec::new();
    for u in &launched.users {
        let mut conn = Conn::new();
        let body = json!({ "user": u.email, "password": u.password.expose() }).to_string();
        let r = conn
            .request(
                &launched.target,
                "POST",
                "/api/v1/auth/login",
                &[("Content-Type", "application/json")],
                Some(body.as_bytes()),
                faults::REQUEST_TIMEOUT,
            )
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let token = r
            .json()
            .pointer("/data/token")
            .and_then(|t| t.as_str())
            .unwrap()
            .to_owned();
        bearers.push(format!("Bearer {token}"));
    }
    let baseline = sampler.sample().await.pg_connections.unwrap_or(0);
    let mut tasks = Vec::new();
    for i in 0..SESSIONS {
        let target = launched.target.clone();
        let bearer = bearers[i % bearers.len()].clone();
        let clock = clock.clone();
        tasks.push(tokio::spawn(async move {
            let mut conn = Conn::new();
            let body = json!({ "limit": 25 }).to_string().into_bytes();
            let mut out = Vec::new();
            for _ in 0..REQUESTS {
                let started = clock.now_ns();
                let status = conn
                    .request(
                        &target,
                        "POST",
                        "/api/v1/resource/task/query",
                        &[
                            ("Authorization", &bearer),
                            ("Content-Type", "application/json"),
                        ],
                        Some(&body),
                        faults::REQUEST_TIMEOUT,
                    )
                    .await
                    .map_or(0, |r| r.status);
                out.push((status, (clock.now_ns() - started) / 1_000_000));
            }
            out
        }));
    }
    let mut statuses = Vec::new();
    let mut latencies = Vec::new();
    for t in tasks {
        for (status, ms) in t.await.unwrap() {
            statuses.push(status);
            latencies.push(ms);
        }
    }
    let failed = statuses.iter().filter(|s| **s != 200).count();
    latencies.sort_unstable();
    let p99 = latencies[(latencies.len() * 99).div_ceil(100).saturating_sub(1)];
    println!(
        "l9 faults: pool exhaustion: {} requests, {failed} failed, p99 {p99} ms, baseline connections {baseline}",
        statuses.len()
    );
    assert_eq!(failed, 0, "queueing behind the pool must not fail requests");
    assert!(
        p99 < faults::REQUEST_TIMEOUT.as_millis() as u64,
        "p99 {p99} ms is not bounded by the request timeout"
    );
    // The pool gives its connections back: after the load there are no more than the pool holds.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let after = sampler.sample().await.pg_connections.unwrap_or(0);
    assert!(
        after <= baseline.max(16) + 4,
        "{after} connections remain after the load (baseline {baseline}): a leak"
    );
    launched.shutdown().await;
}

// --- no-PostgreSQL tests ----------------------------------------------------------------------------------

#[test]
fn the_faults_profile_is_the_tier_m_shape_and_scales_in_integers() {
    let profile = load_profile();
    assert_eq!(profile.sessions, 200);
    assert_eq!(profile.target_milli_tps(), 200_000);
    let small = scaled(&profile, 100, 90);
    assert_eq!(small.sessions, 20);
    assert_eq!(small.target_milli_tps(), 20_000);
    assert_eq!(small.steady_seconds, 90);
    let tiny = scaled(&profile, 1, 90);
    assert_eq!(tiny.sessions, 2);
    assert_eq!(tiny.target_milli_tps(), 1_000);
}

#[test]
fn statuses_are_classified_and_a_500_is_unexpected() {
    use faults::Attempt;
    assert_eq!(faults::classify(201), Attempt::Acked);
    assert_eq!(faults::classify(409), Attempt::Exists);
    for retryable in [429, 502, 503, 504] {
        assert_eq!(faults::classify(retryable), Attempt::Retryable);
    }
    assert_eq!(faults::classify(500), Attempt::Unexpected(500));
    assert_eq!(faults::classify(400), Attempt::Unexpected(400));
}

#[test]
fn recovery_needs_a_streak_of_acknowledged_attempts_after_the_fault() {
    use faults::{Attempt, Ledger};
    let mut ledger = Ledger::default();
    let seq = [
        (1000, Attempt::Acked),
        (2000, Attempt::Retryable),
        (3000, Attempt::TimedOut),
        (4000, Attempt::Acked),
        (5000, Attempt::Acked),
        (6000, Attempt::Dropped),
        (7000, Attempt::Acked),
        (8000, Attempt::Acked),
        (9000, Attempt::Exists),
    ];
    ledger.attempts = seq.to_vec();
    assert_eq!(ledger.recovered_at(2000, 3), Some(9000));
    assert_eq!(ledger.recovered_at(2000, 2), Some(5000));
    assert_eq!(ledger.recovered_at(9500, 1), None);
    ledger.attempts.push((10_000, Attempt::Unexpected(500)));
    assert_eq!(ledger.unexpected(), vec![500]);
}

#[test]
fn the_ledger_check_names_a_lost_acknowledged_write() {
    use faults::{Fault, Ledger, check_ledger};
    let mut ledger = Ledger::default();
    ledger.acked.insert("A".to_owned());
    ledger.acked.insert("B".to_owned());
    ledger.unacked.insert("C".to_owned());
    check_ledger(
        Fault::PgRestart,
        &ledger,
        &["A".to_owned(), "B".to_owned(), "C".to_owned()],
    );
    check_ledger(Fault::PgRestart, &ledger, &["A".to_owned(), "B".to_owned()]);
    let lost = std::panic::catch_unwind(|| {
        check_ledger(Fault::PgRestart, &ledger, &["A".to_owned()]);
    });
    assert!(
        lost.is_err(),
        "a lost acknowledged write must fail the check"
    );
    let twice = std::panic::catch_unwind(|| {
        check_ledger(
            Fault::DiskFull,
            &ledger,
            &["A".to_owned(), "B".to_owned(), "B".to_owned()],
        );
    });
    assert!(twice.is_err(), "a duplicate must fail the check");
}

#[test]
fn readiness_transitions_are_found_and_the_disk_fill_is_computed() {
    let series = [
        (0, 200),
        (250, 200),
        (500, 503),
        (750, 0),
        (1000, 200),
        (1250, 200),
    ];
    assert_eq!(faults::transitions(&series, 0), (Some(500), Some(1000)));
    assert_eq!(faults::transitions(&series, 1000), (None, None));
    assert_eq!(faults::fill_target_bytes(1_000, 100, 95), 850);
    assert_eq!(faults::fill_target_bytes(1_000, 990, 95), 0);
    let df = "Filesystem 1-blocks Used Available Capacity Mounted on\n/dev/loop0 104857600 2097152 102760448 2% /mnt/small\n";
    assert_eq!(faults::parse_df(df), Some((104_857_600, 2_097_152)));
    assert_eq!(faults::parse_df("one line"), None);
    assert_eq!(faults::database_of("l9-scale-abc"), "l9_scale_abc");
}

#[test]
fn faults_that_need_a_prepared_runner_say_so() {
    assert!(preflight(Fault::PgConnectionKill).is_none());
    assert!(preflight(Fault::ClockSkew).is_some());
    assert!(!Fault::PgConnectionKill.expects_not_ready());
    assert!(Fault::PgRestart.expects_not_ready());
}
